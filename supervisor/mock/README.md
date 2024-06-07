# Treadmill Mock Supervisor

This supervisor does not start any actual host environment, and is
intended for development purposes only. It starts a
[puppet](../../puppet) daemon as a subprocess and simply pretends to
run an actual host.

You can use it with the CLI connector to experiment with Treadmill
locally. The mock supervisor requires a puppet binary to be built
beforehand:

1. Build the puppet binary:

   ```
   [treadmill]$ cd puppet

   [treadmill/puppet]$ cargo build
       Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.05s
   ```

2. Run the mock supervisor, passing the built puppet binary:

   ```
   [treadmill]$ cd supervisor/mock

   [treadmill/supervisor/mock]$ cargo run -- -c config.example.toml -p ../../target/debug/tml-puppet
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.08s
     Running `treadmill/code/target/debug/tml-mock-supervisor -c config.example.toml -p ../../target/debug/tml-puppet`
   ```

3. You're greeted with a CLI prompt. You can create a new job with a
   randomly generated job ID by running `job new`:

   ```
   17:03:15 [INFO] Treadmill Mock Supervisor, Hello World!
   >  job new
   job:cb5fee67-ae55-41fc-abfd-8c68126b5543 ?
   ```

4. Once you select a job, you can control it using the `start`,
   `stop`, and related commands:

   ```
   job:cb5fee67-ae55-41fc-abfd-8c68126b5543 >  start
   17:05:10 [INFO] Requesting start of new job cb5fee67-ae55-41fc-abfd-8c68126b5543, ssh keys: []
   17:05:10 [INFO] Supervisor provides job state for job cb5fee67-ae55-41fc-abfd-8c68126b5543: Starting {
       stage: Starting,
       status_message: None,
   }
   17:05:10 [INFO] Opened control socket UNIX SeqPacket listener on "/run/user/1000/.tmpVvfv9c/S.tml-unix-seqpacket"
   17:05:10 [INFO] Supervisor provides job state for job cb5fee67-ae55-41fc-abfd-8c68126b5543: Starting {
       stage: Booting,
       status_message: None,
   }
   17:05:10 [INFO] Attempting auto-discovery of UNIX SeqPacket control socket endpoint...
   17:05:10 [INFO] Discovered UNIX SeqPacket control socket endpoint from environment variable: "/run/user/1000/.tmpVvfv9c/S.tml-unix-seqpacket"
   17:05:10 [INFO] Puppet started, waiting for supervisor events. Exit with CTRL+C
   17:05:10 [INFO] Received unhandled puppet event: 0, Ready
   job:cb5fee67-ae55-41fc-abfd-8c68126b5543 ?
   ```

   Currently, both the mock supervisor and puppet processes write to the same
   stdout/stderr file descriptors, which can mess up your console output.

   TODO: either cancel and redraw the prompt whenever there has been new output,
   or provide an option to write puppet output to a different file (descriptor),
   observable in a different terminal.
