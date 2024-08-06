# Switchboard CLI

Switchboard CLI is a command-line interface tool for interacting with the Switchboard API. It provides functionality for user authentication, job management, and system interactions.

## Features

- User authentication (login)
- Job management:
  - Enqueue new jobs with various parameters
  - Check job status
  - Cancel jobs
- Configurable via command-line arguments or config file

## Usage

```
switchboard-cli [OPTIONS] <SUBCOMMAND>
```

### Options

- `-c, --config <FILE>`: Sets a custom config file
- `-u, --api-url <URL>`: Sets the API URL directly
- `--log`: Enable logging

### Subcommands

1. Login

   ```
   switchboard-cli login <USERNAME> <PASSWORD>
   ```

2. Job Management

   - Enqueue a job:

     ```
     switchboard-cli job enqueue <SUPERVISOR_ID> <IMAGE_ID> [OPTIONS]
     ```

     Options for job enqueue:

     - `--ssh-keys <KEYS>`: Comma-separated list of SSH public keys
     - `--restart-count <COUNT>`: Remaining restart count
     - `--rendezvous-servers <SERVERS>`: JSON array of rendezvous server specifications
     - `--parameters <PARAMS>`: JSON object of job parameters
     - `--tag-config <CONFIG>`: Tag configuration
     - `--override-timeout <TIMEOUT>`: Override timeout in seconds

   - Check job status:

     ```
     switchboard-cli job status <JOB_ID>
     ```

   - Cancel a job:
     ```
     switchboard-cli job cancel <JOB_ID>
     ```

## Configuration

The CLI can be configured using a TOML file. By default, it looks for `~/.switchboard_config.toml`, but you can specify a custom path using the `-c` option.

Example configuration:

```toml
[api]
url = "https://api.switchboard.example.com"
```

## Building

To build the project:

```
cargo build
```

## Examples

1. Login:

   ```
   switchboard-cli login myusername mypassword
   ```

2. Enqueue a job with all options:

   ```
   switchboard-cli job enqueue 123e4567-e89b-12d3-a456-426614174000 fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210 \
     --ssh-keys "ssh-rsa AAAAB3NzaC1yc2E...,ssh-rsa AAAAB3NzaC1yc2E..." \
     --restart-count 3 \
     --rendezvous-servers '[{"client_id":"uuid","server_base_url":"http://example.com","auth_token":"token"}]' \
     --parameters '{"key1":{"value":"value1","secret":false},"key2":{"value":"value2","secret":true}}' \
     --tag-config 'some_tag_config' \
     --override-timeout 3600
   ```

3. Check job status:

   ```
   switchboard-cli job status 123e4567-e89b-12d3-a456-426614174000
   ```

4. Cancel a job:
   ```
   switchboard-cli job cancel 123e4567-e89b-12d3-a456-426614174000
   ```
