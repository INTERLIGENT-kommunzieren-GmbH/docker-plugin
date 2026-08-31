# IK Docker Control [![Tests](https://github.com/INTERLIGENT-kommunzieren-GmbH/docker-control/actions/workflows/tests.yml/badge.svg)](https://github.com/INTERLIGENT-kommunzieren-GmbH/docker-control/actions/workflows/tests.yml)

A Docker command for controlling the `ik` Docker stack, providing an easy way to manage your Docker containers and perform common operations like building, starting, stopping, and accessing the containers.

## Installation

To install the plugin, simply install it via homebrew.

### Example:

```bash
brew install INTERLIGENT-kommunzieren-GmbH/tap/docker-control
```

This will install and making it accessible with the `docker-control` command.

## Development and Testing

This project is implemented in Rust. To build the plugin from source:

```bash
cargo build --release
```

To run the unit tests:

```bash
cargo nextest run
```

### Installation

If you do not have homebrew installed, install it:

```bash
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
```

To install the native binary via homebrew:

```bash
brew install INTERLIGENT-kommunzieren-GmbH/tap/docker-control
```

## Usage

To use the plugin, invoke `docker-control <command>`.

### Global Options

#### `--dir` / `-d <directory>`
Specify the project directory (default: current directory).

```bash
docker-control --dir /path/to/project <command>
```

### Available Commands

#### `add-deploy-config`
Add deployment configuration for environments.

```bash
docker-control add-deploy-config
```

#### `build [options]`
Build the Docker containers for the project. Accepts all docker-compose build options.

```bash
docker-control build
docker-control build --no-cache
```

#### `console [container] [-- command...]`
Open a bash shell inside a container. Defaults to the `php` container if no container name is provided. For the `php` container, opens as `www-data` user. Everything after a `--` runs as a one-shot command in that container instead of opening a shell, and exits with the command's own status code.

```bash
docker-control console
docker-control console php
docker-control console db
docker-control console -- composer install
docker-control console db -- mysql -e 'show databases'
```

#### `create-control-script <name>`
Create a custom control script with the specified name.

```bash
docker-control create-control-script my-command
```

#### `deploy <env>`
Deploy a selected release to the specified environment. The release/tag is selected interactively from available options. Includes comprehensive error handling, configuration validation, and deployment confirmation.

```bash
docker-control deploy production
docker-control deploy staging
```

#### `help`
Show help message with all available commands and comprehensive project status information including git repository state, deployment configuration, and Docker container status.

```bash
docker-control help
```

#### `init`
Initialize an empty directory with the project template, creating a `.env` file and setting up the PHP version and database port. Only works in empty directories.

```bash
docker-control init
```

#### `merge`
Merge release branch to main using selective cherry-pick workflow. Excludes release-specific commits (those with "release:" prefix) and provides interactive conflict resolution with merge tool support. Each commit is pushed immediately after successful cherry-pick.

```bash
docker-control merge
```

#### `module`
Check a vendor module out for local development, or start a new one. `module create <vendor/name>` scaffolds a new module (`src/`, `git init` on `main`, interactive `composer init`, PSR-4 autoload) and links it — it also adds the app's `require`, which `link` never does, since nothing requires a brand-new module. `module link <vendor/name>` moves the module's git clone from `htdocs/vendor/` to `htdocs/modules/`, adds a Composer `path` repository pinned to the version already installed, and symlinks it back into `vendor/` — so edits survive `composer install`. `module unlink` restores the normal vendor install, keeping the checkout unless `--purge` is passed. `module list` shows which modules are linked. Requires the containers to be running; Composer runs inside the `php` container.

```bash
docker-control module list
docker-control module create acme/widget
docker-control module link acme/widget
docker-control module unlink acme/widget
```

#### `pull`
Pull the latest Docker images for the project.

```bash
docker-control pull
```

#### `pull-ingress`
Pull the latest ingress-related Docker images.

```bash
docker-control pull-ingress
```

#### `release`
Create a new release branch with automated versioning and composer.lock generation. Includes comprehensive error handling, user feedback, and displays the created release information upon completion.

```bash
docker-control release
```

#### `restart`
Restart the project containers (stops and starts them).

```bash
docker-control restart
```

#### `restart-ingress`
Restart the ingress containers (stops and starts them).

```bash
docker-control restart-ingress
```

#### `setacl`
Re-apply host and container ACL permissions on `htdocs` without restarting containers. Use this if file permissions between the host and container get out of sync. Requires the project containers to already be running.

```bash
docker-control setacl
```

#### `show-running`
Show all running projects managed by the Docker plugin.

```bash
docker-control show-running
```

#### `start`
Start the project containers in detached mode.

```bash
docker-control start
```

#### `start-ingress`
Start the ingress containers in detached mode.

```bash
docker-control start-ingress
```

#### `status`
Show the status of the project containers.

```bash
docker-control status
```

#### `status-ingress`
Show the status of the ingress containers.

```bash
docker-control status-ingress
```

#### `stop`
Stop the project containers.

```bash
docker-control stop
```

#### `stop-ingress`
Stop the ingress containers.

```bash
docker-control stop-ingress
```

#### `update`
Update the project with the current template, creating a backup of the existing files, then restart containers.

```bash
docker-control update
```

#### `version`
Show version information for the CLI plugin.

```bash
docker-control version
```

### Custom Commands

The plugin supports custom commands that can be created using the `create-control-script` command. These commands are stored in the `control-scripts` directory of your project and can be executed using `docker-control <command-name>`.

Custom commands will appear in the help output with their descriptions. To set a description for your custom command, modify the echo statement in the `_desc_` section of your script.

Example of a custom command script:

```bash
#!/bin/bash
set -e

# Your command implementation here
echo "Custom command executed"

exit 0
```

### Command Name Clashes

If a custom command's name matches a built-in `docker-control` command (e.g. a script
named `build.sh`), you will be prompted to choose whether the built-in command or your
custom script should run. Hitting Enter/Escape keeps the built-in behavior (today's
default).

To make your script always win without prompting, add an `_override_` block:

```bash
if [[ "$1" == "_override_" ]]; then
    echo "true"
    exit 0
fi
```

`docker-control create-control-script` will offer to add this block for you.

### Project Management

The plugin requires projects to be managed by the Docker control plugin (identified by a `.managed-by-docker-control` file). Most commands will check for this file and exit with an error if the current directory is not a managed project.

### Deployment Configuration

The plugin supports deployment to multiple environments through JSON configuration files (`.deploy.json`). The configuration file can be located in either:

- `htdocs/.docker-control/.deploy.json` (preferred location)
- `.deploy.json` (project root fallback)

The JSON configuration format provides:

- **Structured configuration**: Well-defined schema with validation
- **Environment metadata**: Descriptions, tags, and ordering
- **Default values**: Configurable defaults for new environments
- **Better error handling**: Clear validation messages
- **Future extensibility**: Easy to add new features

Example `.deploy.json` structure:

```json
{
  "version": "1.0",
  "environments": {
    "production": {
      "branch": "env/production",
      "user": "deploy",
      "domain": "production.example.com",
      "serviceRoot": "/var/www/html",
      "description": "Production environment - stable releases only",
      "tags": ["production", "critical"]
    },
    "staging": {
      "branch": "env/staging",
      "user": "deploy",
      "domain": "staging.projects.interligent.com",
      "serviceRoot": "/var/www/html",
      "description": "Staging environment for testing",
      "tags": ["staging", "testing"]
    }
  },
  "environmentOrder": ["production", "staging"],
  "defaults": {
    "serviceRoot": "/var/www/html",
    "domainSuffix": ".projects.interligent.com"
  }
}
```

#### Configuration Fields

The JSON format supports the following configuration fields:

- **Environment-specific branch mappings**: Default branch for each environment
- **User and domain settings**: SSH credentials and target servers for each environment
- **Service root paths**: Deployment directory on target servers
- **Environment metadata**: Descriptions and categorization

The `merge` command uses this configuration to automatically merge branches between environments in the correct order.

### Release Management

The `release` command provides automated release branch creation with:

- Automatic semantic versioning (major.minor.x format)
- Composer.lock generation for releases
- Version updates in composer.json
- Git worktree management for safe release preparation
- Comprehensive error handling and user feedback

### Project Status Information

When you run `docker-control help`, you'll see comprehensive project status information including:

#### Project Directory Status
- **Current project directory path**
- **Plugin management status**: Whether the project is managed by the Docker control plugin
- **Helpful guidance**: Commands to initialize unmanaged projects

#### Git Repository Status
- **Repository state**: Whether the project is a git repository
- **Current branch**: Active branch name with tracking information
- **Working directory status**: Indicates uncommitted changes
- **Remote tracking**: Shows configured remote repositories

#### Deployment Configuration Status
- **Configuration file status**: Whether JSON deployment configuration exists and is valid
- **Configured environments**: List of available deployment environments
- **Configuration validation**: Alerts for malformed JSON configuration files

#### Docker Container Status
- **Docker availability**: Whether Docker is installed and running
- **Container status**: Number of project containers and their running state
- **Quick actions**: Suggested commands based on current container state

#### Status Indicators
- **✓ Green checkmarks**: Properly configured and working features
- **✗ Red X marks**: Missing or broken configurations
- **○ Yellow circles**: Neutral states (e.g., stopped containers)

### Enhanced Merge Workflow

The `merge` command now uses an advanced cherry-pick workflow:

#### Selective Cherry-Picking
- **Smart filtering**: Automatically excludes release-specific commits (those with "release:" prefix)
- **Commit isolation**: Uses git worktrees to isolate merge operations from your working directory
- **Individual processing**: Each commit is cherry-picked and pushed individually for better tracking

#### Interactive Conflict Resolution
- **Automatic conflict detection**: Immediately identifies merge conflicts
- **Merge tool integration**: Launches your configured git merge tool for conflict resolution
- **Retry mechanism**: Allows multiple attempts at conflict resolution without losing progress
- **User choice**: Option to abort or retry when conflicts remain unresolved

#### Real-time Feedback
- **Progress tracking**: Shows which commits are being processed
- **Immediate push**: Each successful cherry-pick is pushed immediately to remote
- **Clear status updates**: Detailed feedback throughout the merge process

### Enhanced Deployment Workflow

The `deploy` command includes significant improvements:

#### Comprehensive Validation
- **Environment validation**: Verifies deployment environment exists and is properly configured
- **Configuration loading**: Robust loading and validation of deployment configuration files
- **Required variables**: Validates all necessary deployment variables are present
- **Interactive release selection**: Choose from available releases/tags for deployment

#### Error Handling and Recovery
- **Detailed error messages**: Specific guidance for different failure scenarios
- **Configuration troubleshooting**: Helpful hints for fixing malformed configurations
- **Graceful fallbacks**: Default values for optional configuration parameters
- **Pre-deployment confirmation**: Review deployment details before execution

#### User Experience Improvements
- **Clear progress indicators**: Step-by-step feedback during deployment process
- **Configuration display**: Shows deployment target and configuration before proceeding
- **Success confirmation**: Clear indication of successful deployment completion