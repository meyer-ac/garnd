use nix::libc;

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

/// Document errors occurring during the destruction of an object after the usual error propagation
/// mechanisms have already been closed down or in other scenarios where propagating the error
/// normally is not possible for some reason.
///
/// This is a "quick and dirty" way to report an error that should only ever be used in that very
/// specific scenario. Its only purpose is to still document the error in some way and continue
/// with the safe deconstruction of the object (or whatever else is going on). Therefore (and due to
/// async safety), errors occurring within this function are dropped silently.
///
/// # ERRORS
/// This function silently fails.
///
/// This function is async safe.
pub fn error_in_brittle_scenario(msg: &str) {
    //eprintln!("ERROR DURING DECONSTRUCTION! {msg}");
    const PRELUDE: *const libc::c_char = c"ERROR IN BRITTLE SCENARIO! ".as_ptr();
    const PRELUDE_LEN: usize = 27;
    unsafe {
        let last_errno = *libc::__errno_location();
        libc::write(libc::STDERR_FILENO, PRELUDE as *const libc::c_void, PRELUDE_LEN);
        libc::write(libc::STDERR_FILENO, msg.as_ptr() as *const libc::c_void, msg.len());
        libc::write(libc::STDERR_FILENO, c"\n".as_ptr() as *const libc::c_void, 1);
        *libc::__errno_location() = last_errno;
    }
}
