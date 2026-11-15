//! `orx board install`: keep `orx up` running on the workstation as a systemd
//! user service, reachable from the user's other devices over Tailscale.

use std::path::Path;

pub const UNIT: &str = "orx-board.service";

/// The unit file. `data_dir` is set only when the user runs orx on a
/// non-default data directory; `path` carries the user's PATH so the service
/// finds the agent CLIs (claude, codex) a ticket's chat runs.
pub fn unit_text(exe: &Path, port: u16, data_dir: Option<&Path>, path: Option<&str>) -> String {
    let mut environment = String::new();
    if let Some(dir) = data_dir {
        environment.push_str(&format!("Environment=\"ORX_DATA_DIR={}\"\n", dir.display()));
    }
    if let Some(path) = path.filter(|path| !path.is_empty()) {
        environment.push_str(&format!("Environment=\"PATH={path}\"\n"));
    }
    format!(
        "[Unit]\n\
         Description=OpenResearch research board (orx up)\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart=\"{exe}\" up --host 0.0.0.0 --port {port} --no-browser\n\
         {environment}\
         Restart=always\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exe = exe.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_restarts_orx_up_on_the_lan_bind_with_the_users_environment() {
        let text = unit_text(
            Path::new("/home/u/bin/orx"),
            4791,
            Some(Path::new("/data/orx")),
            Some("/usr/bin:/home/u/.local/bin"),
        );
        assert!(
            text.contains(
                "ExecStart=\"/home/u/bin/orx\" up --host 0.0.0.0 --port 4791 --no-browser\n"
            ),
            "{text}"
        );
        assert!(text.contains("Environment=\"ORX_DATA_DIR=/data/orx\"\n"));
        assert!(text.contains("Environment=\"PATH=/usr/bin:/home/u/.local/bin\"\n"));
        assert!(text.contains("Restart=always\n") && text.contains("WantedBy=default.target\n"));
        let plain = unit_text(Path::new("/o"), 4791, None, None);
        assert!(!plain.contains("Environment="));
    }
}
