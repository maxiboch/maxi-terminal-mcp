use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    PowerShellCore,
    PowerShell,
    Cmd,
    Sh,
    Nushell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageManager {
    Brew,
    Apt,
    Dnf,
    Pacman,
    Zypper,
    Apk,
    Winget,
    Choco,
    Scoop,
}

impl Shell {
    pub fn executable_name(&self) -> &'static str {
        match self {
            Shell::Bash => "bash",
            Shell::Zsh => "zsh",
            Shell::Fish => "fish",
            Shell::PowerShellCore => "pwsh",
            Shell::PowerShell => "powershell",
            Shell::Cmd => "cmd",
            Shell::Sh => "sh",
            Shell::Nushell => "nu",
        }
    }

    pub fn command_args(&self, cmd: &str) -> Vec<String> {
        match self {
            Shell::Bash | Shell::Zsh | Shell::Fish | Shell::Sh | Shell::Nushell => {
                vec!["-c".to_string(), cmd.to_string()]
            }
            Shell::PowerShellCore | Shell::PowerShell => {
                vec![
                    "-NoProfile".to_string(),
                    "-NonInteractive".to_string(),
                    "-Command".to_string(),
                    cmd.to_string(),
                ]
            }
            Shell::Cmd => vec!["/C".to_string(), cmd.to_string()],
        }
    }

    pub fn all() -> &'static [Shell] {
        &[
            Shell::Bash,
            Shell::Zsh,
            Shell::Fish,
            Shell::PowerShellCore,
            Shell::PowerShell,
            Shell::Cmd,
            Shell::Sh,
            Shell::Nushell,
        ]
    }

    pub fn install_command(&self, pm: PackageManager) -> Option<&'static str> {
        match (self, pm) {
            (Shell::Bash, PackageManager::Brew) => Some("brew install bash"),
            (Shell::Bash, PackageManager::Apt) => Some("sudo apt-get install -y bash"),
            (Shell::Bash, PackageManager::Dnf) => Some("sudo dnf install -y bash"),
            (Shell::Bash, PackageManager::Pacman) => Some("sudo pacman -S --noconfirm bash"),
            (Shell::Bash, PackageManager::Apk) => Some("apk add bash"),

            (Shell::Zsh, PackageManager::Brew) => Some("brew install zsh"),
            (Shell::Zsh, PackageManager::Apt) => Some("sudo apt-get install -y zsh"),
            (Shell::Zsh, PackageManager::Dnf) => Some("sudo dnf install -y zsh"),
            (Shell::Zsh, PackageManager::Pacman) => Some("sudo pacman -S --noconfirm zsh"),
            (Shell::Zsh, PackageManager::Apk) => Some("apk add zsh"),
            (Shell::Zsh, PackageManager::Winget) => Some("winget install zsh"),

            (Shell::Fish, PackageManager::Brew) => Some("brew install fish"),
            (Shell::Fish, PackageManager::Apt) => Some("sudo apt-get install -y fish"),
            (Shell::Fish, PackageManager::Dnf) => Some("sudo dnf install -y fish"),
            (Shell::Fish, PackageManager::Pacman) => Some("sudo pacman -S --noconfirm fish"),
            (Shell::Fish, PackageManager::Apk) => Some("apk add fish"),
            (Shell::Fish, PackageManager::Winget) => Some("winget install fish"),
            (Shell::Fish, PackageManager::Choco) => Some("choco install fish -y"),

            (Shell::PowerShellCore, PackageManager::Brew) => Some("brew install powershell"),
            (Shell::PowerShellCore, PackageManager::Apt) => {
                Some("sudo apt-get install -y powershell")
            }
            (Shell::PowerShellCore, PackageManager::Dnf) => Some("sudo dnf install -y powershell"),
            (Shell::PowerShellCore, PackageManager::Winget) => {
                Some("winget install Microsoft.PowerShell")
            }
            (Shell::PowerShellCore, PackageManager::Choco) => {
                Some("choco install powershell-core -y")
            }
            (Shell::PowerShellCore, PackageManager::Scoop) => Some("scoop install pwsh"),

            (Shell::Nushell, PackageManager::Brew) => Some("brew install nushell"),
            (Shell::Nushell, PackageManager::Winget) => Some("winget install nushell"),
            (Shell::Nushell, PackageManager::Choco) => Some("choco install nushell -y"),
            (Shell::Nushell, PackageManager::Scoop) => Some("scoop install nu"),

            _ => None,
        }
    }
}

impl PackageManager {
    pub fn executable_name(&self) -> &'static str {
        match self {
            PackageManager::Brew => "brew",
            PackageManager::Apt => "apt-get",
            PackageManager::Dnf => "dnf",
            PackageManager::Pacman => "pacman",
            PackageManager::Zypper => "zypper",
            PackageManager::Apk => "apk",
            PackageManager::Winget => "winget",
            PackageManager::Choco => "choco",
            PackageManager::Scoop => "scoop",
        }
    }

    pub fn all() -> &'static [PackageManager] {
        &[
            PackageManager::Brew,
            PackageManager::Apt,
            PackageManager::Dnf,
            PackageManager::Pacman,
            PackageManager::Zypper,
            PackageManager::Apk,
            PackageManager::Winget,
            PackageManager::Choco,
            PackageManager::Scoop,
        ]
    }
}

pub fn detect_available_shells() -> Vec<(Shell, PathBuf)> {
    let mut found = Vec::new();
    for shell in Shell::all() {
        if let Ok(path) = which::which(shell.executable_name()) {
            found.push((*shell, path));
        }
    }
    found
}

pub fn detect_default_shell() -> Option<(Shell, PathBuf)> {
    if let Ok(shell_env) = std::env::var("SHELL") {
        let path = PathBuf::from(&shell_env);
        if path.exists() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                let shell = match name {
                    "bash" => Some(Shell::Bash),
                    "zsh" => Some(Shell::Zsh),
                    "fish" => Some(Shell::Fish),
                    "pwsh" => Some(Shell::PowerShellCore),
                    "nu" => Some(Shell::Nushell),
                    "sh" => Some(Shell::Sh),
                    _ => None,
                };
                if let Some(s) = shell {
                    return Some((s, path));
                }
            }
        }
    }

    let priority: &[Shell] = if cfg!(windows) {
        &[Shell::PowerShellCore, Shell::PowerShell, Shell::Cmd]
    } else {
        &[Shell::Zsh, Shell::Bash, Shell::Fish, Shell::Sh]
    };

    for shell in priority {
        if let Ok(path) = which::which(shell.executable_name()) {
            return Some((*shell, path));
        }
    }
    None
}

pub fn detect_available_package_managers() -> Vec<(PackageManager, PathBuf)> {
    let mut found = Vec::new();
    for pm in PackageManager::all() {
        if let Ok(path) = which::which(pm.executable_name()) {
            found.push((*pm, path));
        }
    }
    found
}

pub fn get_install_instructions(shell: Shell) -> HashMap<PackageManager, &'static str> {
    let mut instructions = HashMap::new();
    for pm in PackageManager::all() {
        if let Some(cmd) = shell.install_command(*pm) {
            instructions.insert(*pm, cmd);
        }
    }
    instructions
}

pub async fn install_shell(shell: Shell) -> Result<PathBuf> {
    let available_pms = detect_available_package_managers();
    if available_pms.is_empty() {
        return Err(anyhow!("No supported package manager found"));
    }

    for (pm, _) in &available_pms {
        if let Some(install_cmd) = shell.install_command(*pm) {
            let (exec, args) = if cfg!(windows) {
                ("cmd", vec!["/C", install_cmd])
            } else {
                ("sh", vec!["-c", install_cmd])
            };

            let status = Command::new(exec)
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .status()
                .await?;

            if status.success() {
                if let Ok(path) = which::which(shell.executable_name()) {
                    return Ok(path);
                }
            }
        }
    }

    Err(anyhow!(
        "Failed to install {} with available package managers",
        shell.executable_name()
    ))
}

pub async fn ensure_shell(shell: Shell) -> Result<PathBuf> {
    if let Ok(path) = which::which(shell.executable_name()) {
        return Ok(path);
    }
    install_shell(shell).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shell_executable_names() {
        assert_eq!(Shell::Bash.executable_name(), "bash");
        assert_eq!(Shell::PowerShellCore.executable_name(), "pwsh");
        assert_eq!(Shell::Cmd.executable_name(), "cmd");
    }

    #[test]
    fn test_shell_command_args() {
        let args = Shell::Bash.command_args("echo hello");
        assert_eq!(args, vec!["-c", "echo hello"]);

        let args = Shell::PowerShellCore.command_args("echo hello");
        assert_eq!(args[0], "-NoProfile");
        assert_eq!(args[3], "echo hello");

        let args = Shell::Cmd.command_args("echo hello");
        assert_eq!(args, vec!["/C", "echo hello"]);
    }

    #[test]
    fn test_detect_available_shells() {
        let shells = detect_available_shells();
        assert!(!shells.is_empty(), "Should find at least one shell");
    }

    #[test]
    fn test_detect_default_shell() {
        let default = detect_default_shell();
        assert!(default.is_some(), "Should find a default shell");
    }

    #[test]
    fn test_install_instructions() {
        let instructions = get_install_instructions(Shell::Fish);
        assert!(!instructions.is_empty());
    }
}
