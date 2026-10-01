//! Recognition of container runtime port-forwarding processes.
//!
//! When a container publishes a port, the host-side listener is often not the
//! container's own process but a runtime helper that forwards traffic into the
//! container network. A port scanner that sees such a helper should resolve
//! the port through the container map instead of reporting the helper itself.

/// Process names of container runtime helpers that listen on published ports
/// on behalf of a container, compared ASCII case-insensitively.
const CONTAINER_PROXY_PROCESSES: &[&str] = &[
    // Docker Engine userland proxy (rootful Linux).
    "docker-proxy",
    // Rootless Docker: RootlessKit's builtin port driver binds in the parent.
    "rootlesskit",
    // Rootless Podman port forwarder and its in-namespace child.
    "rootlessport",
    "rootlessport-child",
    // User-mode networking stacks that bind published ports themselves
    // (rootless Podman and rootless Docker with these port handlers).
    "slirp4netns",
    "pasta",
    "pasta.avx2",
    // Docker Desktop (macOS, Windows, Linux) and its older VPNKit forwarder.
    "com.docker.backend",
    "com.docker.vpnkit",
    "vpnkit",
    // WSL 2 localhost relay used by Docker Desktop and Podman machine on Windows.
    "wslrelay",
    // Podman machine user-mode network proxy (macOS and Windows).
    "gvproxy",
    // Lima host agent, which forwards guest ports for Lima, Colima and
    // Rancher Desktop on macOS.
    "limactl",
];

/// Lengths the kernel truncates a long process name to: Linux keeps 15 bytes
/// of `comm` (16 including the trailing NUL), and macOS keeps 16 bytes
/// (`MAXCOMLEN`), so `com.docker.backend` is seen as `com.docker.back` on
/// Linux and `com.docker.backe` on macOS.
const TRUNCATED_NAME_LENS: [usize; 2] = [15, 16];

/// Check whether a process name belongs to a container runtime port proxy.
///
/// These processes hold a published port on the host and forward it into a
/// container, so the port should be attributed to the container rather than
/// to the helper. Recognized names:
///
/// | Runtime                              | Process names                                   |
/// | ------------------------------------ | ----------------------------------------------- |
/// | Docker Engine (rootful)              | `docker-proxy`                                  |
/// | Docker Engine (rootless)             | `rootlesskit`, `slirp4netns`                    |
/// | Podman (rootless)                    | `rootlessport`, `rootlessport-child`, `slirp4netns`, `pasta`, `pasta.avx2` |
/// | Docker Desktop                       | `com.docker.backend`, `com.docker.vpnkit`, `vpnkit`, `wslrelay` |
/// | Podman machine                       | `gvproxy`, `wslrelay`                           |
/// | Lima, Colima, Rancher Desktop (macOS)| `limactl`                                       |
///
/// The comparison is ASCII case-insensitive and ignores a trailing `.exe`
/// (in any case), so Windows image names such as `com.docker.backend.exe`
/// match. A name of exactly 15 or 16 bytes that is the start of a longer
/// known name also matches, because Linux truncates process names to 15
/// bytes (`rootlessport-ch`) and macOS to 16 (`com.docker.backe`). Shorter
/// prefixes never match.
///
/// Generic tools that can also forward ports, such as `ssh` or `socat`, are
/// deliberately not recognized: they are used for far more than container
/// port forwarding. The list may grow in minor releases as runtimes add or
/// rename helpers.
///
/// # Examples
///
/// ```
/// use nanodock::is_container_proxy_process;
///
/// assert!(is_container_proxy_process("docker-proxy"));
/// assert!(is_container_proxy_process("COM.DOCKER.BACKEND.EXE"));
/// assert!(is_container_proxy_process("rootlessport-ch"));
/// assert!(is_container_proxy_process("com.docker.backe"));
/// assert!(!is_container_proxy_process("nginx"));
/// ```
#[must_use]
pub fn is_container_proxy_process(name: &str) -> bool {
    let name = strip_exe_suffix(name);
    CONTAINER_PROXY_PROCESSES
        .iter()
        .any(|known| name.eq_ignore_ascii_case(known) || is_truncated_name_of(name, known))
}

/// Whether `name` is `known` truncated by the kernel: `name` is exactly one
/// of the [`TRUNCATED_NAME_LENS`] long, `known` is longer, and `known`
/// starts with `name`.
fn is_truncated_name_of(name: &str, known: &str) -> bool {
    TRUNCATED_NAME_LENS.contains(&name.len())
        && known.len() > name.len()
        && known
            .as_bytes()
            .get(..name.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(name.as_bytes()))
}

/// Strip a trailing `.exe` (ASCII case-insensitive) from a process name.
fn strip_exe_suffix(name: &str) -> &str {
    const SUFFIX: &str = ".exe";
    name.len()
        .checked_sub(SUFFIX.len())
        .filter(|&split| {
            name.get(split..)
                .is_some_and(|suffix| suffix.eq_ignore_ascii_case(SUFFIX))
        })
        .and_then(|split| name.get(..split))
        .unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_every_listed_proxy() {
        for name in CONTAINER_PROXY_PROCESSES {
            assert!(
                is_container_proxy_process(name),
                "{name} is listed as a container proxy"
            );
        }
    }

    #[test]
    fn covers_the_names_portlens_checked() {
        for name in [
            "wslrelay",
            "com.docker.backend",
            "vpnkit",
            "docker-proxy",
            "rootlessport",
        ] {
            assert!(
                is_container_proxy_process(name),
                "{name} was recognized by portlens and must stay recognized"
            );
        }
    }

    #[test]
    fn ignores_case_and_exe_suffix() {
        assert!(
            is_container_proxy_process("wslrelay.exe"),
            "Windows image names carry .exe"
        );
        assert!(
            is_container_proxy_process("COM.DOCKER.BACKEND.EXE"),
            "suffix and name compare case-insensitively"
        );
        assert!(
            is_container_proxy_process("gvproxy.Exe"),
            "mixed-case suffix is stripped"
        );
        assert!(
            is_container_proxy_process("ROOTLESSPORT"),
            "upper-case name matches"
        );
    }

    #[test]
    fn accepts_linux_truncated_names() {
        assert!(
            is_container_proxy_process("rootlessport-ch"),
            "rootlessport-child as truncated by the kernel"
        );
        assert!(
            is_container_proxy_process("com.docker.back"),
            "com.docker.backend as truncated by the kernel"
        );
        assert!(
            !is_container_proxy_process("rootlessport-c"),
            "a shorter prefix is not a kernel truncation"
        );
        assert!(
            !is_container_proxy_process("docker-pro"),
            "a prefix of a short name is not a truncation"
        );
    }

    #[test]
    fn accepts_macos_truncated_names() {
        for name in ["com.docker.backe", "com.docker.vpnki", "rootlessport-chi"] {
            assert_eq!(name.len(), 16, "{name} is a 16-byte macOS name");
            assert!(
                is_container_proxy_process(name),
                "{name} is a known proxy as truncated by macOS"
            );
        }
        assert!(
            is_container_proxy_process("COM.DOCKER.BACKE"),
            "truncated names compare case-insensitively"
        );
        for name in [
            "com.docker.backx",
            "com.docker.bac",
            "com.docker.backendx",
            "rootlessport-chx",
            "slirp4netns-xyzw",
        ] {
            assert!(
                !is_container_proxy_process(name),
                "{name:?} is not a truncation of a known proxy name"
            );
        }
    }

    #[test]
    fn rejects_unrelated_and_generic_processes() {
        for name in [
            "nginx",
            "postgres.exe",
            "ssh",
            "socat",
            "docker",
            "podman",
            "dockerd",
            "",
            ".exe",
            "exe",
            "pasta.exe.exe",
            "\u{e9}abc",
        ] {
            assert!(
                !is_container_proxy_process(name),
                "{name:?} is not a container port proxy"
            );
        }
    }

    #[test]
    fn strip_exe_suffix_handles_edge_cases() {
        assert_eq!(strip_exe_suffix("vpnkit.exe"), "vpnkit", "suffix removed");
        assert_eq!(strip_exe_suffix("VPNKIT.EXE"), "VPNKIT", "case kept");
        assert_eq!(strip_exe_suffix(".exe"), "", "bare suffix strips to empty");
        assert_eq!(
            strip_exe_suffix("exe"),
            "exe",
            "too short to carry a suffix"
        );
        assert_eq!(
            strip_exe_suffix("my.executable"),
            "my.executable",
            "suffix must be at the end"
        );
        assert_eq!(
            strip_exe_suffix("\u{e9}exe"),
            "\u{e9}exe",
            "a split inside a multi-byte character is not a suffix"
        );
    }
}
