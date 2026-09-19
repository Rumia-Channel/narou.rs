# Web UI self-update under systemd (issue #23)

On Linux, an updater spawned from a `narou_rs` systemd service remains in
that service's cgroup even when it calls `setsid`. With the default
`KillMode=control-group`, systemd can terminate it when the Web process
exits. `Restart=always` may additionally start an old binary before the zip
has been applied.

Recent builds detect a systemd service in `/proc/self/cgroup`. In that case,
the Web UI asks `systemd-run` to launch the **installed** updater in its own
transient service. The updater runs without `--restart` (compatible with
updaters already included in older release ZIPs); on successful completion,
the transient service calls `systemctl restart <original-unit>` (or
`systemctl --user restart` for user units). No change to the original
`Restart=` or `KillMode=` settings is required.

Requirements:

- `systemd-run` and `systemctl` must be available.
- The Web service's user must have permission to start a transient service
  through the appropriate systemd manager. The transient service must be
  able to replace files in the installation directory.
- When `systemd-run` refuses the handoff, the API returns an error and the Web
  process **does not exit**. Examine `update.log` and the transient unit's
  journal (unit name `narou-rs-update-<Web-PID>`).

The fix can only take effect after an updated `narou_rs` executable is
installed. For an existing v0.3.6 installation that cannot self-update, stop
`narou.service` using `systemctl stop`, install the latest official release
ZIP's files into `/opt/narou` using your existing deployment procedure,
ensure `narou_rs_updater.new` is installed as executable
`narou_rs_updater`, and start the unit again. Keep the Web library's working
directory and its `.narou/` files intact. Do not replace a running binary
while the service is active.

This describes the update handoff on Linux only; outside a detected systemd
service, the existing detached-updater behavior is unchanged.
