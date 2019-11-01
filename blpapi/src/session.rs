use crate::{
    correlation_id::CorrelationId,
    element::Element,
    event::{Event, EventType},
    name::Name,
    ref_data::RefData,
    request::Request,
    service::Service,
    session_options::SessionOptions,
    try_, Error,
};
use blpapi_sys::*;
use std::collections::HashMap;
use std::{ffi::CString, ptr};

const MAX_PENDING_REQUEST: usize = 1024;
const MAX_REFDATA_FIELDS: usize = 400;
const MAX_HISTDATA_FIELDS: usize = 25;

pub struct Session {
    ptr: *mut blpapi_Session_t,
    // keep a handle of the options (not sure if it should be droped or not)
    //_options: SessionOptions,
    correlation_count: u64,
}

impl Session {
    /// Create a new session
    fn from_options(options: SessionOptions) -> Self {
        //TODO: check if null values are ok!
        let handler = None;
        let dispatcher = ptr::null_mut();
        let user_data = ptr::null_mut();
        let ptr = unsafe { blpapi_Session_create(options.0, handler, dispatcher, user_data) };
        Session {
            ptr,
            //_options: options,
            correlation_count: 0,
        }
    }

    /// Start the session
    pub fn start(&mut self) -> Result<(), Error> {
        let res = unsafe { blpapi_Session_start(self.ptr) };
        try_(res)
    }

    /// Stop the session
    pub fn stop(&mut self) -> Result<(), Error> {
        let res = unsafe { blpapi_Session_stop(self.ptr) };
        try_(res)
    }

    /// Open service
    pub fn open_service(&mut self, service: &str) -> Result<(), Error> {
        let service = CString::new(service).unwrap();
        let res = unsafe { blpapi_Session_openService(self.ptr, service.as_ptr()) };
        try_(res)
    }

    /// Get opened service
    pub fn get_service(&self, service: &str) -> Result<Service, Error> {
        let name = CString::new(service).unwrap();
        let mut service = ptr::null_mut();
        let res =
            unsafe { blpapi_Session_getService(self.ptr, &mut service as *mut _, name.as_ptr()) };
        try_(res)?;
        Ok(Service(service))
    }

    /// Send request
    pub fn send(
        &mut self,
        request: Request,
        correlation_id: Option<CorrelationId>,
    ) -> Result<CorrelationId, Error> {
        let mut correlation_id = correlation_id.unwrap_or_else(|| self.new_correlation_id());
        let identity = ptr::null_mut();
        let event_queue = ptr::null_mut();
        let request_label = ptr::null_mut();
        let request_label_len = 0;
        unsafe {
            let res = blpapi_Session_sendRequest(
                self.ptr,
                request.ptr,
                &mut correlation_id.0 as *mut _,
                identity,
                event_queue,
                request_label,
                request_label_len,
            );
            try_(res)?;
            Ok(correlation_id)
        }
    }

    fn new_correlation_id(&mut self) -> CorrelationId {
        let id = CorrelationId::new_u64(self.correlation_count);
        self.correlation_count += 1;
        id
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe { blpapi_Session_destroy(self.ptr) }
    }
}

/// A wrapper for session which only show sync fn
pub struct SessionSync(Session);

impl SessionSync {
    /// Create a new `SessionSync` from a `SessionOptions`
    pub fn from_options(options: SessionOptions) -> Self {
        SessionSync(Session::from_options(options))
    }

    /// Create a new `SessionSync` with default options and open refdata service
    pub fn new() -> Result<Self, Error> {
        let mut session = Self::from_options(SessionOptions::new());
        session.start()?;
        session.open_service("//blp/refdata")?;
        Ok(session)
    }

    /// Send request and get `Events` iterator
    pub fn send(
        &mut self,
        request: Request,
        correlation_id: Option<CorrelationId>,
    ) -> Result<Events, Error> {
        let _id = (&mut *self as &mut Session).send(request, correlation_id)?;
        Ok(Events::new(self))
    }

    /// Request for next event, optionally waiting timeout_ms if there is no event
    pub fn next_event(&mut self, timeout_ms: Option<u32>) -> Result<Event, Error> {
        let mut event = ptr::null_mut();
        let timeout = timeout_ms.unwrap_or(0);
        unsafe {
            let res = blpapi_Session_nextEvent(self.0.ptr, &mut event as *mut _, timeout);
            try_(res)?;
            Ok(Event(event))
        }
    }

    /// Get reference data for `RefData` items
    ///
    /// # Note
    /// For ease of use, you can activate the **derive** feature.
    ///
    /// # Example
    ///
    /// ```
    /// # #[cfg(feature = "derive")]
    /// # {
    /// use blpapi::{RefData, session::SessionSync};
    ///
    /// // use the **derive** feature to automatically convert field names into bloomberg fields
    /// #[derive(Default, RefData)]
    /// struct EquityData {
    ///     ticker: String,
    ///     crncy: String,
    ///     market_status: Option<String>,
    /// }
    ///
    /// let mut session = SessionSync::new().unwrap();
    /// let securities: &[&str] = &[ /* list of security tickers */ ];
    ///
    /// let maybe_equities = session.ref_data::<_, EquityData>(securities);
    /// # }
    /// ```
    pub fn ref_data<I, R>(&mut self, securities: I) -> Result<HashMap<String, R>, Error>
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
        R: RefData,
    {
        let service = self.get_service("//blp/refdata")?;
        let mut ref_data: HashMap<String, R> = HashMap::new();
        let mut iter = securities.into_iter();

        // TODO: should be consts
        let security_data = Name::new("securityData");
        let security_name = Name::new("security");
        let field_data = Name::new("fieldData");
        let security_error = Name::new("securityError");
        let securities = Name::new("securities");
        let fields_name = Name::new("fields");

        // split request as necessary to comply with bloomberg size limitations
        for fields in R::FIELDS.chunks(MAX_REFDATA_FIELDS) {
            loop {
                // create new request
                let mut request = service.create_request("ReferenceDataRequest")?;

                // add next batch of securities and exit loop if empty
                let mut is_empty = true;
                for security in iter.by_ref().take(MAX_PENDING_REQUEST / fields.len()) {
                    request.append_named(&securities, security.as_ref())?;
                    is_empty = false;
                }
                if is_empty {
                    break;
                }

                // add fields
                for field in fields {
                    request.append_named(&fields_name, *field)?;
                }

                // send request
                for event in self.send(request, None)? {
                    for message in event?.messages().map(|m| m.element()) {
                        if let Some(securities) = message.get_named_element(&security_data) {
                            for security in securities.values::<Element>() {
                                let ticker = security
                                    .get_named_element(&security_name)
                                    .and_then(|s| s.get_at(0))
                                    .unwrap_or_else(|| String::new());
                                if security.has_named_element(&security_error) {
                                    break;
                                }
                                let entry = ref_data.entry(ticker).or_default();
                                if let Some(fields) = security.get_named_element(&field_data) {
                                    for field in fields.elements() {
                                        entry.on_field(&field.string_name(), &field);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(ref_data)
    }

    /// Get historical data
    ///
    /// # Note
    /// For ease of use, you can activate the **derive** feature.
    ///
    /// # Example
    ///
    /// ```
    /// # #[cfg(feature = "derive")]
    /// # {
    /// use blpapi::{RefData, session::{SessionSync, HistOptions}};
    ///
    /// // use the **derive** feature to automatically convert field names into bloomberg fields
    /// #[derive(Default, RefData)]
    /// struct Price {
    ///     px_last: f64,
    /// }
    ///
    /// let mut session = SessionSync::new().unwrap();
    /// let securities: &[&str] = &[ /* list of security tickers */ ];
    ///
    /// let options = HistOptions::new("20190101", "20191231");
    /// let prices = session.hist_data::<_, Price>(securities, options);
    /// # }
    /// ```
    pub fn hist_data<I, R>(
        &mut self,
        securities: I,
        options: HistOptions,
    ) -> Result<HashMap<String, TimeSerie<R>>, Error>
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
        R: RefData,
    {
        let service = self.get_service("//blp/refdata")?;
        let mut ref_data: HashMap<String, TimeSerie<R>> = HashMap::new();
        let mut iter = securities.into_iter();

        // TODO: should be consts
        let security_data = Name::new("securityData");
        let security_name = Name::new("security");
        let field_data = Name::new("fieldData");
        let security_error = Name::new("securityError");
        let securities = Name::new("securities");
        let fields_name = Name::new("fields");

        // split request as necessary to comply with bloomberg size limitations
        for fields in R::FIELDS.chunks(MAX_HISTDATA_FIELDS) {
            loop {
                // create new request
                let mut request = service.create_request("HistoricalDataRequest")?;

                // add next batch of securities and exit loop if empty
                let mut is_empty = true;
                for security in iter.by_ref().take(MAX_PENDING_REQUEST / fields.len()) {
                    request.append_named(&securities, security.as_ref())?;
                    is_empty = false;
                }
                if is_empty {
                    break;
                }

                // add fields
                for field in fields {
                    request.append_named(&fields_name, *field)?;
                }

                options.apply(&mut request)?;

                // send request
                for event in self.send(request, None)? {
                    for message in event?.messages().map(|m| m.element()) {
                        if let Some(securities) = message.get_named_element(&security_data) {
                            for security in securities.values::<Element>() {
                                let ticker = security
                                    .get_named_element(&security_name)
                                    .and_then(|s| s.get_at(0))
                                    .unwrap_or_else(|| String::new());
                                if security.has_named_element(&security_error) {
                                    break;
                                }
                                if let Some(fields) = security.get_named_element(&field_data) {
                                    let entry = ref_data.entry(ticker).or_insert_with(|| {
                                        let len = fields.num_values();
                                        TimeSerie::<_>::with_capacity(len)
                                    });
                                    for points in fields.values::<Element>() {
                                        let mut value = R::default();
                                        for field in points.elements() {
                                            let name = &field.string_name();
                                            if name == "date" {
                                                #[cfg(feature = "dates")]
                                                entry
                                                    .dates
                                                    .extend(field.get_at::<chrono::NaiveDate>(0));
                                                #[cfg(not(feature = "dates"))]
                                                entry.dates.extend(field.get_at(0));
                                            } else {
                                                value.on_field(name, &field);
                                            }
                                        }
                                        entry.values.push(value);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(ref_data)
    }
}

impl std::ops::Deref for SessionSync {
    type Target = Session;
    fn deref(&self) -> &Session {
        &self.0
    }
}

impl std::ops::DerefMut for SessionSync {
    fn deref_mut(&mut self) -> &mut Session {
        &mut self.0
    }
}

/// An iterator over messages
pub struct Events<'a> {
    session: &'a mut SessionSync,
    exit: bool,
    session_terminated: Name,
    session_startup_failure: Name,
}

impl<'a> Events<'a> {
    fn new(session: &'a mut SessionSync) -> Self {
        let session_terminated = Name::new("SesssionTerminated");
        let session_startup_failure = Name::new("SessionStartupFailure");
        Events {
            session,
            session_startup_failure,
            session_terminated,
            exit: false,
        }
    }
    fn try_next(&mut self) -> Result<Option<Event>, Error> {
        if self.exit {
            return Ok(None);
        }
        loop {
            let event = self.session.next_event(None)?;
            let event_type = event.event_type();
            match event_type {
                EventType::PartialResponse => return Ok(Some(event)),
                EventType::Response => {
                    self.exit = true;
                    return Ok(Some(event));
                }
                EventType::SessionStatus => {
                    if event
                        .messages()
                        .map(|m| m.message_type())
                        .any(|m| m == self.session_terminated || m == self.session_startup_failure)
                    {
                        return Ok(None);
                    }
                }
                _ => (),
            }
        }
    }
}

impl<'a> Iterator for Events<'a> {
    type Item = Result<Event, Error>;
    fn next(&mut self) -> Option<Result<Event, Error>> {
        self.try_next().transpose()
    }
}

/// Options for historical data request
#[derive(Debug, Default)]
pub struct HistOptions {
    /// Start date yyyyMMdd
    pub start_date: String,
    /// end date yyyyMMdd
    pub end_date: String,
    /// periodicity_adjustment (ACTUAL...)
    pub periodicity_adjustment: Option<String>,
    /// periodicity_selection (DAILY, MONTHLY, QUARTERLY, SEMIANNUALLY, ANNUALLY)
    pub periodicity_selection: Option<String>,
    /// max_data_points
    pub max_data_points: Option<i32>,
}

impl HistOptions {
    /// Crate a new historical options
    pub fn new<S: Into<String>, E: Into<String>>(start_date: S, end_date: E) -> Self {
        HistOptions {
            start_date: start_date.into(),
            end_date: end_date.into(),
            ..HistOptions::default()
        }
    }

    /// Set periodicity_adjustment
    pub fn with_periodicity_adjustment(mut self, periodicity_adjustment: String) -> Self {
        self.periodicity_adjustment = Some(periodicity_adjustment);
        self
    }

    /// Set periodicity_adjustment
    pub fn with_periodicity_selection(mut self, periodicity_selection: String) -> Self {
        self.periodicity_selection = Some(periodicity_selection);
        self
    }

    /// Set max points
    pub fn with_max_points(mut self, max_data_points: i32) -> Self {
        self.max_data_points = Some(max_data_points);
        self
    }

    fn apply(&self, request: &mut Request) -> Result<(), Error> {
        let mut element = request.element();
        element.set("startDate", &self.start_date[..])?;
        element.set("endDate", &self.end_date[..])?;
        if let Some(periodicity_selection) = &self.periodicity_selection {
            element.set("periodicitySelection", &periodicity_selection[..])?;
        }
        if let Some(periodicity_adjustment) = &self.periodicity_adjustment {
            element.set("periodicityAdjustment", &periodicity_adjustment[..])?;
        }
        if let Some(max_data_points) = self.max_data_points {
            element.set("maxDataPoints", max_data_points)?;
        }
        Ok(())
    }
}

#[derive(Default, Debug)]
#[cfg(feature = "dates")]
pub struct TimeSerie<R> {
    pub dates: Vec<chrono::NaiveDate>,
    pub values: Vec<R>,
}

#[derive(Default)]
#[cfg(not(feature = "dates"))]
pub struct TimeSerie<R> {
    pub dates: Vec<crate::datetime::Datetime>,
    pub values: Vec<R>,
}

impl<R> TimeSerie<R> {
    /// Create a new timeseries with given capacity
    pub fn with_capacity(capacity: usize) -> Self {
        TimeSerie {
            dates: Vec::with_capacity(capacity),
            values: Vec::with_capacity(capacity),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_request() -> Result<(), Error> {
        let mut session = SessionOptions::new()
            .with_server_host("localhost")?
            .with_server_port(8194)?
            .sync();

        //session.start()?;
        //session.open_service("//blp/refdata")?;

        Ok(())
    }
}
