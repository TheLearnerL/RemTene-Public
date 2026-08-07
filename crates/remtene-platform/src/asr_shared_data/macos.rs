use std::path::PathBuf;

use objc2_foundation::{NSFileManager, NSString};

use super::AsrSharedDataError;

pub(super) fn resolve_container(group_identifier: &str) -> Result<PathBuf, AsrSharedDataError> {
    let manager = NSFileManager::defaultManager();
    let identifier = NSString::from_str(group_identifier);
    let url = manager
        .containerURLForSecurityApplicationGroupIdentifier(&identifier)
        .ok_or(AsrSharedDataError::ContainerUnavailable)?;
    let path = url
        .path()
        .map(|path| PathBuf::from(path.to_string()))
        .ok_or(AsrSharedDataError::ContainerUnavailable)?;
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(AsrSharedDataError::ContainerUnavailable)
    }
}
