# Switchboard CLI

Switchboard CLI is a command-line interface tool for interacting with the Switchboard API. It provides functionality for user authentication and job management.

## Features

- User authentication (login)
- Job management:
  - Enqueue new jobs with various parameters
  - Check job status
  - Cancel jobs
- Configurable via command-line arguments or config file

## Installation

The CLI tool is now named `tml`. Ensure it's in your system path or reference it directly using `./tml`.

## Usage

```
./tml [OPTIONS] <SUBCOMMAND>
```

### Options

- `-c, --config <FILE>`: Sets a custom config file
- `--log`: Enable detailed logging (Debug level)

### Subcommands

1. Login

   ```
   ./tml login <USERNAME> <PASSWORD>
   ```

2. Job Management

   - Enqueue a job:

     ```
     ./tml job enqueue <IMAGE_ID> [OPTIONS]
     ```

     Options for job enqueue:

     - `--request-id <REQUEST_ID>`: Request ID (UUID)
     - `--ssh-keys <KEYS>`: Comma-separated list of SSH public keys
     - `--restart-count <COUNT>`: Remaining restart count
     - `--rendezvous-servers <SERVERS>`: JSON array of rendezvous server specifications
     - `--parameters <PARAMS>`: JSON object of job parameters
     - `--tag-config <CONFIG>`: Tag configuration
     - `--override-timeout <TIMEOUT>`: Override timeout in seconds

   - Check job status:

     ```
     ./tml job status <JOB_ID>
     ```

   - Cancel a job:
     ```
     ./tml job cancel <JOB_ID>
     ```

## Configuration

The CLI can be configured using a TOML file. You can specify the config file path using the `-c` option.

Example configuration:

```toml
[api]
url = "http://api.treadmill.ci"
```

## Building

To build the project:

```
cargo build --package tml-switchboard-cli
```

## Examples

1. Login:

   ```
   ./tml -c cli_config.toml login fake_user1 FAKEFAKE
   ```

2. Enqueue a job with all options:

   ```
   ./tml -c cli_config.toml job enqueue 46ebc6946f7c4a10922bf1f539cd7351ce8670781e081d18babf1affdef6f577 \
     --request-id "$(uuidgen)" \
     --ssh-keys "ssh-rsa AAAAB3NzaC1yc2E...,ssh-ed25519 AAAAC3NzaC1lZDI1NTE5..." \
     --restart-count 3 \
     --rendezvous-servers '[{"client_id":"12345678-1234-5678-1234-567812345678","server_base_url":"http://example.com","auth_token":"exampletoken"}]' \
     --parameters '{"key1":{"value":"value1","secret":false},"key2":{"value":"value2","secret":true}}' \
     --tag-config 'test_tag_config' \
     --override-timeout 3600
   ```

3. Check job status:

   ```
   ./tml -c cli_config.toml job status <JOB_ID>
   ```

4. Cancel a job:
   ```
   ./tml -c cli_config.toml job cancel <JOB_ID>
   ```

## Logging

To enable detailed logging, simply add the `--log` flag to your command:

```
./tml --log -c cli_config.toml job enqueue <IMAGE_ID>
```

This will output debug-level logs, which can be helpful for troubleshooting.

## Notes

- The image ID should be a 64-character hexadecimal string.
- Job IDs are UUIDs.
- When using the `--rendezvous-servers` option, provide a valid JSON array of server specifications.
- The --parameters option requires a JSON string in the format described in the "Job Parameters Format" section above. Each parameter must have a "value" (as a string) and a "secret" (as a boolean) field.
- The --parameters option requires a specific JSON format:
  ```json
  {
    "key1": { "value": "value1", "secret": false },
    "key2": { "value": "value2", "secret": true }
  }
  ```
  Each parameter must have a "value" (as a string) and a "secret" (as a boolean) field.

For more detailed information about each command and its options, use the `--help` flag with any command or subcommand.
