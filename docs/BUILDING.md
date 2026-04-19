# Building the RPM

The RPM is built with [`cargo-generate-rpm`][cgr], a Rust-native RPM
builder that reads metadata from `Cargo.toml`. No `rpmbuild` or
`%spec` file required.

[cgr]: https://github.com/cat-in-136/cargo-generate-rpm

## Prerequisites

One-time install on the build host:

```
cargo install cargo-generate-rpm
```

The build host must match the target ABI. For the Hetzner CX32 target
(Rocky Linux 10, x86_64) the build host should be either the same
Rocky 10 machine or Rocky 9 with a compatible glibc. Rocky 9.7 (our
dev machines) builds binaries that run on Rocky 10 without issue.

## Build

From the workspace root:

```
cargo build --release -p rt-daemon
cargo generate-rpm -p crates/rt-daemon
```

The output is:

```
target/generate-rpm/rt-daemon-0.1.0-1.el9.x86_64.rpm
```

(The `el9` suffix reflects the default target; the RPM is
forward-compatible with Rocky 10.)

## Install on the target

Copy to the Hetzner machine and install with `dnf`:

```
scp target/generate-rpm/rt-daemon-*.rpm hetzner-host:/tmp/
ssh hetzner-host 'sudo dnf install -y /tmp/rt-daemon-*.rpm'
```

`dnf install` (not `rpm -i`) resolves the `systemd` and
`shadow-utils` dependencies declared in `Cargo.toml`.

## Verify

After install:

```
rpm -ql rt-daemon
rpm -q --scripts rt-daemon   # inspect pre/post install scriptlets
systemctl cat rt-daemon.service
id razortrade
ls -la /etc/razortrade /var/lib/razortrade
```

The service is installed but deliberately not enabled or started —
operator must provision credentials per the packaged README
(`/usr/share/doc/razortrade/README.md`) before the first
`systemctl enable --now`.

## Version bump

Bump `[workspace.package].version` in the root `Cargo.toml`, then
rebuild. The version is the only thing that changes in the RPM
filename; release (`-1.el9`) stays at `1` unless explicitly
overridden via `--set-metadata release=N`.
