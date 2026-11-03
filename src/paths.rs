use std::path::PathBuf;

/// %LOCALAPPDATA%\OmniPilot
pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    base.join("OmniPilot")
}

pub fn models_dir() -> PathBuf {
    data_dir().join("models")
}

pub fn runtime_dir() -> PathBuf {
    data_dir().join("runtime")
}

pub fn scenarios_dir() -> PathBuf {
    data_dir().join("scenarios")
}

pub fn logs_dir() -> PathBuf {
    data_dir().join("logs")
}

pub fn settings_file() -> PathBuf {
    data_dir().join("settings.json")
}
