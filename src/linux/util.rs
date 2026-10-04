macro_rules! unwrap_or_report_failure {
    ($expr:expr, $client_fd:expr, $response_type:ident) => {
        match $expr {
            ::std::result::Result::Ok(res) => res,
            ::std::result::Result::Err(e) => {
                let mut errors: ::std::vec::Vec<
                    ::garnshared::error_types::DetailedError,
                > = ::std::vec![e];
                let response = $response_type::serialize_internal_error();
                if let ::std::result::Result::Err(e) = ::nix::sys::socket::send(
                    $client_fd,
                    response.as_bytes(),
                    ::nix::sys::socket::MsgFlags::empty(),
                )
                .add_metadata()
                {
                    errors.push(e);
                }
                return ::std::result::Result::Err(errors);
            }
        }
    };
}

pub(crate) use unwrap_or_report_failure;
