pub struct ConnectionString {

}

impl ConnectionString {
    pub fn new(connstr: &str) -> Self {
        Self {}
    }
}

impl From<String> for ConnectionString {
    fn from(connstr: String) -> Self {
        ConnectionString::new(connstr.as_str())
    }
}

impl From<&str> for ConnectionString {
    fn from(connstr: &str) -> Self {
       ConnectionString::new(connstr)
    }
}