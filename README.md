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

#### `build`
Build the Docker containers for the project.

```bash
docker control build
```

#### `console <container>`
Open a bash shell inside a container. Defaults to the `php` container if no container name is provided.

```bash
docker control console php
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
Currently not implemented.

```bash
docker control update
```

#### `version`
Show version information for the CLI plugin.

```bash
docker control version
```

### Custom Commands
