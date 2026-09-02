macro_rules! unwrap_or_report_failure {
    ($expr:expr, $client_fd:expr, $response_type:ident, $err_map:expr) => {
        match $expr {
            Ok(res) => res,
            Err(e) => {
                let mut errors: Vec<SendableError> = vec![$err_map(e)];
                let response = $response_type::InternalError.serialize();
                if let Err(e) = send($client_fd, response.as_bytes(), MsgFlags::empty()) {
                    errors.push(Box::new(e));
                }
                return Err(errors);
            }
        }
    };
    ($expr:expr, $client_fd:expr, $response_type:ident) => {
        unwrap_or_report_failure!($expr, $client_fd, $response_type, (|x| x))
    };
}

pub(crate) use unwrap_or_report_failure;
