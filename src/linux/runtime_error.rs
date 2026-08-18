use crate::constants;
use std::error::Error;
use std::fmt;

// todo: more detailed
#[derive(Debug)]
pub enum RuntimeError {
    UserNonexistent,
    RunAsWrongUser,
    RunAsWrongGroup,
    RunWithRootGroup,
    RunWithCapabilities,
    MayObtainNewPrivileges,
    SecureBitsNotSet,
    CanGainPrivileges,
    ServiceAlreadyRunning,
    WelcomeSocketFailed,
    GetPageSizeFailed,
    ResourceNameAlreadyInUse,
    ResourceAlignmentLargerThanPage,
    ResourceTooLargeForPage,
    ResourceNotFound,
    ResourceTypeMismatch
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UserNonexistent => write!(f, "unable to find user '{}'", constants::USER_NAME),
            Self::RunAsWrongUser => write!(f, "service must be run as user '{}' (real, effective, saved and file system)", constants::USER_NAME),
            Self::RunAsWrongGroup => write!(f, "service must be run as user '{}' (real, effective, saved and file system)", constants::GROUP_NAME),
            Self::RunWithRootGroup => write!(f, "service must not be run with 'root' as a supplementary group"),
            Self::RunWithCapabilities => write!(f, "service must not be run with any permitted or bounding capabilities"),
            Self::MayObtainNewPrivileges => write!(f, "service must be run with the 'no new privs' attribute"),
            Self::SecureBitsNotSet => write!(f, "service must be run with all secure bits and all locks set except for 'keep_caps'"),
            Self::CanGainPrivileges => write!(f, "service must not be able to gain privileges"),
            Self::ServiceAlreadyRunning => write!(f, "service is either already running or another process impersonates it"),
            Self::WelcomeSocketFailed => write!(f, "welcome socket broke down unexpectedly"),
            Self::GetPageSizeFailed => write!(f, "could not determine the system's page size"),
            Self::ResourceNameAlreadyInUse => write!(f, "a resource with the same name already exists"),
            Self::ResourceAlignmentLargerThanPage => write!(f, "tried to move a resource into share memory whose alignment is larger than the page size"),
            Self::ResourceTooLargeForPage => write!(f, "tried to move a resource into shared memory which is larger than a whole page"),
            Self::ResourceNotFound => write!(f, "tried to access a non-existent resource"),
            Self::ResourceTypeMismatch => write!(f, "tried to access a resource of the wrong type"),
        }
    }
}
impl Error for RuntimeError {}