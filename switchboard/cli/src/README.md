# Switchboard CLI

Switchboard CLI is a command-line interface tool for interacting with the Switchboard API. It provides functionality for user authentication, job management, and system interactions.

## Features

- User authentication (login)
- Job management:
  - Enqueue new jobs
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
     switchboard-cli job enqueue <SUPERVISOR_ID> <IMAGE_ID>
     ```
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
