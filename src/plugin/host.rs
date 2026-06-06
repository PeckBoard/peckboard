use extism::{CurrentPlugin, UserData, Val};

/// Host function: peckboard_log
/// Allows plugins to write to Peckboard's log.
/// Input: JSON string { "level": "info"|"warn"|"error", "message": "..." }
pub fn peckboard_log(
    _plugin: &mut CurrentPlugin,
    _inputs: &[Val],
    _outputs: &mut [Val],
    _user_data: UserData<()>,
) -> Result<(), extism::Error> {
    // The actual message is passed via the plugin's input buffer.
    // For now this is a stub that will be wired up when we have
    // the full host function interface.
    Ok(())
}

/// Host function: peckboard_get_config
/// Allows plugins to read configuration values.
/// Input: JSON string { "key": "..." }
/// Output: JSON string with the config value
pub fn peckboard_get_config(
    _plugin: &mut CurrentPlugin,
    _inputs: &[Val],
    _outputs: &mut [Val],
    _user_data: UserData<()>,
) -> Result<(), extism::Error> {
    Ok(())
}
