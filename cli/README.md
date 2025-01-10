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
   ./tml login
   ```

   Or, optionally:

   ```
   ./tml login <USERNAME> [<PASSWORD>]
   ```

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

## Login Options

Treadmill CLI allows you to log in using **several** different methods, **in order of precedence**:

1. **Positional Arguments**

   - `<USERNAME> [<PASSWORD>]`  
     Examples:

   ```
   ./tml login ben mypassword
   ```

   ```
   ./tml login ben
   ```

   In the second example, you’ll be prompted for `mypassword`.

2. **Environment Variables**

   - `TML_USER`
   - `TML_PASSWORD`  
     Example:

   ```
   export TML_USER=ben
   export TML_PASSWORD=supersecret
   ./tml login
   ```

   If these environment variables are set, the CLI will use them **unless** the above command-line arguments override them.

3. **Interactive Prompt**
   - If username and/or password aren’t provided by flags, positional arguments, or environment variables, the CLI will prompt you interactively.

### Examples

- **Fully Interactive**:

  ```
  ./tml login
  ```

  - Prompts for username, then password.

- **Username Positional + Prompt for Password**:

  ```
  ./tml login ben
  ```

  - Username is `ben`
  - Prompts for password.

- **Username & Password Positional**:

  ```
  ./tml login ben mypassword
  ```

  - Username is `ben`
  - Password is `mypassword`
  - No prompt needed.

- **Environment Variables**:
  ```
  export TML_USER=ben
  export TML_PASSWORD=supersecret
  ./tml login
  ```
  - Username is `ben`
  - Password is `supersecret`
  - No prompt needed.

**Note**: The CLI always checks for **flags first**, **then** any **positional arguments**, **then** environment variables, **finally** falling back to prompts for whichever piece is still missing. This flexibility makes the CLI suitable for both interactive and CI-based automation.

## Configuration

The CLI can be configured using a TOML file. You can specify the config file path using the `-c` option.

Example configuration:

```toml
ssh_keys = "ssh-rsa AAAAB3NzaC1yc2E..., ssh-ed25519 AAAAC3NzaC1lZDI1NTE5..."

[api]
url = "https://swb.treadmill.ci"
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
   ./tml login
   ```

   CLI will prompt you safely for your username and password if they are not provided by any other means.

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
