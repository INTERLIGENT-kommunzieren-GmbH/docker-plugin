# IK Docker Control CLI Plugin

A Docker CLI plugin for controlling the `ik` Docker stack, providing an easy way to manage your Docker containers and perform common operations like building, starting, stopping, and accessing the containers.

## Installation

To install the plugin, simply run the `install.sh` script, which will automatically set up the necessary files and make the plugin executable.

### Example:

```bash
curl -sL https://raw.githubusercontent.com/INTERLIGENT-kommunzieren-GmbH/docker-plugin/main/install.sh | bash
```

This will install the plugin into `~/.ik-docker`, making it accessible with the `docker control` command.

## Usage

To use the plugin, invoke `docker control <command>`.

### Available Commands

#### `add-deploy-config`
Add deployment configuration for environments.

```bash
docker control add-deploy-config
```

#### `build`
Build the Docker containers for the project.

```bash
docker control build
```

#### `cap <env>`
Deploy via capistrano to the specified environment.

```bash
docker control cap production
```

#### `console <container>`
Open a bash shell inside a container. Defaults to the `php` container if no container name is provided.

```bash
docker control console php
```

#### `create-control-script <name>`
Create a custom control script with the specified name.

```bash
docker control create-control-script my-command
```

#### `deploy <env> <branch>`
Deploy the specified branch to the specified environment.

```bash
docker control deploy production main
```

#### `help`
Show this help message with all available options and commands.

```bash
docker control help
```

#### `init`
Initialize an empty directory with the project template, creating a `.env` file and setting up the PHP version and database port.

```bash
docker control init
```

#### `merge`
Automatic branch merging between environments.

```bash
docker control merge
```

#### `pull`
Pull the latest Docker images for the project.

```bash
docker control pull
```

#### `pull-ingress`
Pull the latest ingress-related Docker images.

```bash
docker control pull-ingress
```

#### `restart`
Restart the project containers.

```bash
docker control restart
```

#### `restart-ingress`
Restart the ingress containers.

```bash
docker control restart-ingress
```

#### `start`
Start the project containers in detached mode.

```bash
docker control start
```

#### `start-ingress`
Start the ingress containers in detached mode.

```bash
docker control start-ingress
```

#### `status`
Show the status of the project containers.

```bash
docker control status
```

#### `status-ingress`
Show the status of the ingress containers.

```bash
docker control status-ingress
```

#### `stop`
Stop the project containers.

```bash
docker control stop
```

#### `stop-ingress`
Stop the ingress containers.

```bash
docker control stop-ingress
```

#### `update`
Update the project with the current template, creating a backup of the existing files.

```bash
docker control update
```

#### `version`
Show version information for the CLI plugin.

```bash
docker control version
```

### Custom Commands

The plugin supports custom commands that can be created using the `create-control-script` command. These commands are stored in the `control-scripts` directory of your project and can be executed using `docker control <command-name>`.

Custom commands will appear in the help output with their descriptions. To set a description for your custom command, modify the echo statement in the `_desc_` section of your script.

Example of a custom command script:

```bash
#!/bin/bash
set -e

. "$LIB_DIR/util-functions.sh"

if [[ "$1" == "_desc_" ]]; then
    # output command description
    echo "My custom command description"

    exit 0
fi

# Your command implementation here
info "Custom command executed"

exit 0
```