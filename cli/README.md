# Treadmill CLI

Treadmill CLI is a command-line interface tool for interacting with the Treadmill test bench system. It provides functionality for user authentication and job management.

## Features

- User authentication (login)
- Job management:
  - Enqueue new jobs with various parameters
  - List all jobs in the queue
  - Check job status
  - Cancel jobs
- Configurable via command-line arguments or config file

## Installation

Ensure the CLI tool is in your system path or reference it directly using `./tml`.

## Usage

```
./tml [OPTIONS] <SUBCOMMAND>
```

### Global Options

- `-c, --config <FILE>`: Sets a custom config file
- `-u, --api-url <URL>`: Sets the API URL directly
- `-v, --verbose`: Enable verbose logging

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

     - `--ssh-keys <KEYS>`: Comma-separated list of SSH public keys
     - `--restart-count <COUNT>`: Remaining restart count
     - `--parameters <PARAMS>`: JSON object of job parameters
     - `--tag-config <CONFIG>`: Tag configuration
     - `--timeout <TIMEOUT>`: Override timeout in seconds

   - List all jobs:

     ```
     ./tml job list
     ```

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
ssh_keys = "ssh-rsa AAAAB3NzaC1yc2E..., ssh-ed25519 AAAAC3NzaC1lZDI1NTE5..."

[api]
url = "https://api.treadmill.ci"
```

## SSH Key Handling

The CLI reads SSH keys from multiple sources:

1. SSH agent
2. Public key files in the user's `.ssh` directory
3. Config file (as shown above)

If no SSH keys are provided via the command-line argument, the CLI will automatically attempt to read keys from these sources.

## Examples

1. Login:

   ```
   ./tml login fake_user1 FAKEFAKE
   ```

2. Enqueue a job:

   ```
   ./tml job enqueue 46ebc6946f7c4a10922bf1f539cd7351ce8670781e081d18babf1affdef6f577 \
     --ssh-keys "ssh-rsa AAAAB3NzaC1yc2E...,ssh-ed25519 AAAAC3NzaC1lZDI1NTE5..." \
     --restart-count 3 \
     --parameters '{"key1":{"value":"value1","secret":false},"key2":{"value":"value2","secret":true}}' \
     --tag-config 'test_tag_config' \
     --timeout 3600
   ```

3. List all jobs:

   ```
   ./tml job list
   ```

4. Check job status:

   ```
   ./tml job status <JOB_ID>
   ```

5. Cancel a job:
   ```
   ./tml job cancel <JOB_ID>
   ```

## Verbose Logging

To enable verbose logging, add the `-v` or `--verbose` flag to your command:

```
./tml -v job enqueue <IMAGE_ID>
```

This will output debug-level logs, which can be helpful for troubleshooting.

## Notes

- The image ID should be a 64-character hexadecimal string.
- Job IDs are UUIDs.
- The `--parameters` option requires a JSON string in the following format:
  ```json
  {
    "key1": { "value": "value1", "secret": false },
    "key2": { "value": "value2", "secret": true }
  }
  ```
  Each parameter must have a "value" (as a string) and a "secret" (as a boolean) field.

For more detailed information about each command and its options, use the `--help` flag with any command or subcommand.
