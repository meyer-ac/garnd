use crate::constants;
use std::error::Error;
use std::fmt;

#[derive(Debug)]
pub enum RuntimeError {
    UserNonexistent,
    GroupNonexistent,
    RunAsWrongUser,
    RunAsWrongGroup,
    RunWithRootGroup,
    RunWithCapabilities,
    MayObtainNewPrivileges,
    SecureBitsNotSet,
    WorkingDirPathInvalidString,
    WorkingDirNonexistent{working_dir: String},
    WorkingDirNotADirectory{working_dir: String},
    WorkingDirOwnedByWrongUser{working_dir: String, owner: String},
    WorkingDirOwnedByWrongGroup{working_dir: String, owner: String},
    WorkingDirWrongPermissions{working_dir: String, permissions: &'static str},
    WorkingDirSetUidBitSet{working_dir: String},
    WorkingDirSetGidBitSet{working_dir: String},
    WorkingDirStickyBitSet{working_dir: String},
    ServiceAlreadyRunning,
    WelcomeSocketFailed,
    GetPageSizeFailed,
    ResourceNameAlreadyInUse{resource_name: String},
    ResourceAlignmentLargerThanPage{page_size: usize, alignment: usize},
    ResourceTooLargeForPage{page_size: usize, size: usize},
    ResourceTypeMismatch{requested_type: &'static str, resource_type: &'static str},
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UserNonexistent => write!(f, "unable to find user '{}'", constants::USER_NAME),
            Self::GroupNonexistent => write!(f, "unable to find group '{}'", constants::GROUP_NAME),
            Self::RunAsWrongUser => write!(
                f,
                "service must be run as user '{}' (real, effective, saved and file system)",
                constants::USER_NAME
            ),
            Self::RunAsWrongGroup => write!(
                f,
                "service must be run as user '{}' (real, effective, saved and file system)",
                constants::GROUP_NAME
            ),
            Self::RunWithRootGroup => write!(
                f,
                "service must not be run with 'root' as a supplementary group"
            ),
            Self::RunWithCapabilities => write!(
                f,
                "service must not be run with any permitted or bounding capabilities"
            ),
            Self::MayObtainNewPrivileges => {
                write!(f, "service must be run with the 'no new privs' attribute")
            }
            Self::SecureBitsNotSet => write!(
                f,
                "service must be run with all secure bits and all locks set except for 'keep_caps'"
            ),
            Self::WorkingDirPathInvalidString => {
                write!(f, "working directory path is not a valid UTF8-string")
            },
            Self::WorkingDirNonexistent{working_dir} => {
                write!(f, "working directory (\"{working_dir}\") does not exist")
            }
            Self::WorkingDirNotADirectory{working_dir} => {
                write!(f, "working directory (\"{working_dir}\") is not a directory")
            }
            Self::WorkingDirOwnedByWrongUser{working_dir, owner} => {
                write!(f, "working directory (\"{working_dir}\") is owned by the wrong user (required owner = {}, actual owner = {owner})", constants::USER_NAME)
            }
            Self::WorkingDirOwnedByWrongGroup{working_dir, owner} => {
                write!(f, "working directory (\"{working_dir}\") is owned by the wrong group (required owner = {}, actual owner = {owner})", constants::GROUP_NAME)
            }
            Self::WorkingDirWrongPermissions{working_dir, permissions} => {
                write!(f, "working directory (\"{working_dir}\") has the wrong permissions (required permissions = {})", *permissions)
            }
            Self::WorkingDirSetUidBitSet{working_dir} => {
                write!(f, "working directory (\"{working_dir}\") has the set-uid bit set")
            }
            Self::WorkingDirSetGidBitSet{working_dir} => {
                write!(f, "working directory (\"{working_dir}\") has the set-gid bit set")
            }
            Self::WorkingDirStickyBitSet{working_dir} => {
                write!(f, "working directory (\"{working_dir}\") has the sticky bit set")
            }
            Self::ServiceAlreadyRunning => write!(
                f,
                "service is either already running or another process impersonates it"
            ),
            Self::WelcomeSocketFailed => write!(f, "welcome socket broke down unexpectedly"),
            Self::GetPageSizeFailed => write!(f, "could not determine the system's page size"),
            Self::ResourceNameAlreadyInUse{resource_name} => {
                write!(f, "a resource with the same name (\"{resource_name}\") already exists")
            }
            Self::ResourceAlignmentLargerThanPage{page_size, alignment} => write!(
                f,
                "tried to move a resource into share memory whose alignment is larger than the page size (page size = {page_size}, alignment = {alignment})"
            ),
            Self::ResourceTooLargeForPage{page_size, size} => write!(
                f,
                "tried to move a resource into shared memory which is larger than a whole page (page size = {page_size}, resource size = {size})"
            ),
            Self::ResourceTypeMismatch{requested_type, resource_type} => write!(f, "tried to access a resource of the wrong type (requested type = {}, resource_type = {})", *requested_type, *resource_type),
        }
    }
}
impl Error for RuntimeError {}
