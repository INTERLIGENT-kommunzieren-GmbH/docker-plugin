#!/bin/bash

function _addDeployConfig() {
    if [[ ! -f "$PROJECT_DIR/.deploy.conf" ]]; then
        createDeployConfig
    else
        addDeployConfig
    fi
}

function checkDir() {
    if [[ ! -f "$PROJECT_DIR"/.managed-by-docker-control-plugin ]]; then
        critical "$PROJECT_DIR not managed by docker control plugin"
        exit 1
    fi
}

function _cap() {
    # shellcheck disable=SC2124
    local PARAMETER="$@"
    dockerCompose run --rm capistrano bash -l -i -c "cap $PARAMETER"
}

function _createControlScript {
    local COMMAND=$1
    if [[ -f "${PROJECT_DIR}/control-scripts/${COMMAND}.sh" ]]; then
        critical "command '$COMMAND' already exists in $PROJECT_DIR"
        exit 1
    else
        cat << EOF | tee "$PROJECT_DIR/control-scripts/${COMMAND}.sh" 1>/dev/null
#!/bin/bash
set -e

. "$LIB_DIR/util-functions.sh"

if [[ "\$1" == "_desc_" ]]; then
    # output command description
    echo "EMPTY DESCRIPTION"

    exit 0
fi

info "WAITING FOR IMPLEMENTATION"

exit 0
EOF
        chmod u+x "$PROJECT_DIR/control-scripts/${COMMAND}.sh"

        text 'command {{ Foreground "14" "'"$COMMAND"'"}} created under {{ Foreground "14" "'"$PROJECT_DIR"'"}}'
    fi
}

function _console() {
    local SERVICE="$1"
    if [[ -z "$SERVICE" ]]; then
        SERVICE=$(select_docker_service)
    fi

    if [[ "$SERVICE" == "help" ]]; then
        sub_headline "Available containers"
        for SERVICE in $(dockerCompose ps --services); do
            info "$SERVICE"
        done
        newline
    elif [[ "$SERVICE" == "php" ]]; then
        dockerCompose exec -itu www-data "$SERVICE" bash
    else
        dockerCompose exec "$SERVICE" bash
    fi
}

function _deploy() {
    if [[ -z "$DEPLOY_ENVS" ]]; then
        if [[ ! -f "$PROJECT_DIR/.deploy.conf" ]]; then
            createDeployConfig
        fi
        . "$PROJECT_DIR/.deploy.conf"
    fi

    local ENV="$1"
    if [[ -z "${DEPLOY_ENVS[$ENV]+set}" ]]; then
        critical "Environment $ENV not configured"
        exit 1
    fi

    local BRANCH
    local IS_MERGE_STOP
    local USER
    local DOMAIN
    local SERSERVICE_ROOT
    eval "${DEPLOY_ENVS[$ENV]}"

    local DEPLOY_BRANCH="${2:-$BRANCH}"
    deploy "$ENV" "$USER" "$DOMAIN" "$SERSERVICE_ROOT" "$DEPLOY_BRANCH"
}

function addDeployConfig() {
    local ENV
    input -n -l "environment" -r ENV

    local BRANCH
    input -l "branch" -d "env/$ENV" -r BRANCH
    local IS_MERGE_STOP="n"
    if [[ -n "$BRANCH" ]]; then
        IS_MERGE_STOP=$(confirm -n "Is this a merge stop?")
    fi

    local USER
    input -n -l "user" -r USER
    local DOMAIN
    input -n -l "domain" -d "$USER.projects.interligent.com" -r DOMAIN
    input -n -l "server root" -d "/var/www/html" -r SERVICE_ROOT

    cat <<EOF | tee -a "$PROJECT_DIR/.deploy.conf" 1>/dev/null
DEPLOY_ENVS["$ENV"]="BRANCH=$BRANCH IS_MERGE_STOP=$IS_MERGE_STOP USER=$USER DOMAIN=$DOMAIN SERVICE_ROOT=$SERVICE_ROOT"
DEPLOY_ENVS_ORDER+=("$ENV")

EOF
}

function createDeployConfig() {
    cat <<EOF | tee "$PROJECT_DIR/.deploy.conf" 1>/dev/null
declare -A DEPLOY_ENVS
declare -A DEPLOY_ENVS_ORDER

EOF
    addDeployConfig
}

function dockerCompose() {
    docker compose --project-directory "$PROJECT_DIR" "$@"
}

function dockerComposeIngress() {
    docker compose --project-directory "$INGRESS_COMPOSE_DIR" -f "$INGRESS_COMPOSE_FILE" "$@"
}

function _help() {
    headline "IK Docker Control $SERVICE"
    newline
    sub_headline "Options"
    info "  $(printf "%-25s\n" "-d|--dir") Project directory (default: current directory)"
    newline
    sub_headline "Commands"
    info "  $(printf "%-25s\n" "add-deploy-config") Add deployment config"
    info "  $(printf "%-25s\n" "build") Build containers"
    info "  $(printf "%-25s\n" "cap <env>") Deploy via capistrano to environment"
    info "  $(printf "%-25s\n" "console <container>") Enter container console (defaults to php)"
    info "  $(printf "%-25s\n" "deploy <env> <branch>") Deploy branch to environment"
    info "  $(printf "%-25s\n" "merge") Automatic branch merging"
    info "  $(printf "%-25s\n" "help") Show this help"
    info "  $(printf "%-25s\n" "init") Initialize empty directory with project template"
    info "  $(printf "%-25s\n" "pull") Pull current container images"
    info "  $(printf "%-25s\n" "pull-ingress") Pull current ingress images"
    info "  $(printf "%-25s\n" "restart") Restart project containers"
    info "  $(printf "%-25s\n" "restart-ingress") Restart ingress containers"
    info "  $(printf "%-25s\n" "start") Start project containers"
    info "  $(printf "%-25s\n" "start-ingress") Start ingress containers"
    info "  $(printf "%-25s\n" "status") Show status of project containers"
    info "  $(printf "%-25s\n" "status-ingress") Show status of ingress containers"
    info "  $(printf "%-25s\n" "stop") Stop project containers"
    info "  $(printf "%-25s\n" "stop-ingress") Stop ingress containers"
    info "  $(printf "%-25s\n" "update") Update project with current template"
    info "  $(printf "%-25s\n" "version") Show version information"
    newline

    if ls "$PROJECT_DIR"/control-scripts/*.sh 1> /dev/null 2>&1; then
        sub_headline "Custom commands"
        for COMMAND in "$PROJECT_DIR"/control-scripts/*.sh; do
            local SHORT_COMMAND
            SHORT_COMMAND=$(basename "$COMMAND" .sh)
            local DESCRIPTION
            DESCRIPTION=$("$COMMAND" _desc_)
            info "  $(printf "%-25s\n" "${SHORT_COMMAND}") ${DESCRIPTION}"
        done
        newline
    fi
}

function _init() {
    info "Creating project template"
    cp -r "$TEMPLATE_DIR"/* "$PROJECT_DIR/"
    mv "$PROJECT_DIR/.gitignore-dist" "$PROJECT_DIR/.gitignore"
    mkdir "$PROJECT_DIR/htdocs"

    local PROJECT_NAME
    PROJECT_NAME=$(input -n -l "Project name")
    local PHP_VERSION
    PHP_VERSION=$(select_php_version)
    local DB_HOST_PORT=""

    for i in {33060..33099}; do
        DB_HOST_PORT=$i
        DB_HOST_PORT_IN_USE=$(ss -tulw | grep -F "*:$DB_HOST_PORT" > /dev/null && echo "yes" || echo "no")
        if [ "$DB_HOST_PORT_IN_USE" == "no" ]; then
            break
        fi
        info "Automatically selected DB_HOST_PORT $DB_HOST_PORT as it seems to be free. Please verify it and adjust accordingly in .env file."
    done
    if [ -z "$DB_HOST_PORT" ]; then
        critical "No empty port found between 33060 and 33099 for external database connection, please select one manually and update your .env file."
        newline
    fi

    cat << EOF | tee "$PROJECT_DIR/.env" 1>/dev/null
BASE_DOMAIN=${PROJECT_NAME}.lvh.me
ENVIRONMENT=development
DB_HOST_PORT=${DB_HOST_PORT}
PHP_VERSION=${PHP_VERSION}
PROJECTNAME=${PROJECT_NAME}
XDEBUG_IP=host.docker.internal
IDE_KEY=${PROJECT_NAME}.lvh.me
EOF

    local CHECKOUT_PROJECT
    CHECKOUT_PROJECT=$(confirm -n "Do you want to checkout a project into htdocs folder?")
    if [ "$CHECKOUT_PROJECT" == "y" ]; then
        local PROJECT_GIT_URL
        PROJECT_GIT_URL=$(input -p "clone url (use ssh link)" -n)
        git checkout "$PROJECT_GIT_URL" "$PROJECT_DIR/htdocs"
    fi
}

function _merge() {
    merge
}

function _update() {
    sub_headline "Updating"
    local BACKUP_DIR
    BACKUP_DIR="${PROJECT_DIR}/backup_$(date +%Y%m%d%H%M%S)"
    mkdir -p "$BACKUP_DIR"
    text 'Creating backup {{ Foreground "14" "'"$(basename "$BACKUP_DIR")"'"}}'
    rsync -a --quiet --exclude "backup_*" --exclude .git --exclude htdocs --exclude volumes "$PROJECT_DIR/" "$BACKUP_DIR/" 1>/dev/null
    info "Updating project with current template"
    rsync -a --quiet --ignore-existing "$TEMPLATE_DIR/" "$PROJECT_DIR/"
    cat "$PROJECT_DIR"/.gitignore-dist >> "$PROJECT_DIR"/.gitignore
    sort -u "$PROJECT_DIR"/.gitignore -o "$PROJECT_DIR"/.gitignore
    rm "$PROJECT_DIR"/.gitignore-dist
    info "Update completed."
    newline
}

function initializePlugin() {
    if [[ "$1" == "docker-cli-plugin-metadata"  ]] || [[ "$DOCKER_CLI_PLUGIN_METADATA" == "1" ]]; then
      cat <<EOF
    {
      "SchemaVersion": "0.1.0",
      "Vendor": "Interligent kommunizieren GmbH",
      "Version": "$2",
      "ShortDescription": "Docker CLI plugin to control ik docker stack",
      "URL": "https://interligent.com"
    }
EOF
      exit 0
    fi
}

function merge() {
        if [[ -z "$DEPLOY_ENVS" ]]; then
            if [[ ! -f "$PROJECT_DIR/.deploy.conf" ]]; then
                createDeployConfig
            fi
            . "$PROJECT_DIR/.deploy.conf"
        fi

        local MERGE_STOPS=("development")
        local MERGE_STOPS_REVERSE=("development")
        local ENV
        local ENV_BRANCHES=("development")
        local ENV_BRANCHES_REVERSE=("development")

        for ENV in "${!DEPLOY_ENVS_ORDER[@]}"; do
            local BRANCH
            local IS_MERGE_STOP
            local USER
            local DOMAIN
            local SERSERVICE_ROOT
            eval "${DEPLOY_ENVS[$ENV]}"

            if [[ "$IS_MERGE_STOP" == "y" ]]; then
                MERGE_STOPS+=("$BRANCH")
                MERGE_STOPS_REVERSE=("$BRANCH" "${MERGE_STOPS[@]}")
            fi

            ENV_BRANCHES+=("$BRANCH")
            ENV_BRANCHES_REVERSE=("$BRANCH" "${ENV_BRANCHES_REVERSE[@]}")
        done

        local MERGE_MENU_MAP
        declare -A MERGE_MENU_MAP=(
            ["quit"]=255
        )
        local MERGE_MENU_ORDER
        declare -a MERGE_MENU_ORDER=(
            "quit"
        )

        local MERGE_STOP
        local PREVIOUS_MERGE_STOP=""
        local MENU_ENTRY
        for MERGE_STOP in "${MERGE_STOPS[@]}"; do
            if [[ -n "$PREVIOUS_MERGE_STOP" ]]; then
                MENU_ENTRY="merge $PREVIOUS_MERGE_STOP up to $MERGE_STOP"
                # shellcheck disable=SC2034
                MERGE_MENU_MAP["$MENU_ENTRY"]="$PREVIOUS_MERGE_STOP:$MERGE_STOP"
                MERGE_MENU_ORDER+=("$MENU_ENTRY")
            fi
            PREVIOUS_MERGE_STOP="$MERGE_STOP"
        done

        MENU_ENTRY="merge ${MERGE_STOPS_REVERSE[0]} down to ${MERGE_STOPS[0]}"
        # shellcheck disable=SC2034
        MERGE_MENU_MAP["$MENU_ENTRY"]="development"
        MERGE_MENU_ORDER+=("$MENU_ENTRY")

        while true; do
            newline

            local ACTION
            ACTION=$(choose "$HEADER" MERGE_MENU_MAP MERGE_MENU_ORDER)
            local EXIT_CODE="$?"

            if [ "$EXIT_CODE" == 255 ]; then
                break
            elif [ "$ACTION" == "development" ]; then
                info "Merging ${ENV_BRANCHES_REVERSE[0]} down to ${ENV_BRANCHES[0]}"
                local BRANCH
                local PREVIOUS_BRANCH=""
                for BRANCH in "${ENV_BRANCHES_REVERSE[@]}"; do
                    git -C "$PROJECT_DIR/htdocs" switch "$BRANCH"
                    git -C "$PROJECT_DIR/htdocs" pull

                    if [[ -n "$PREVIOUS_BRANCH" ]]; then
                        git -C "$PROJECT_DIR/htdocs" merge "$PREVIOUS_BRANCH"
                        git -C "$PROJECT_DIR/htdocs" push
                    fi

                    PREVIOUS_BRANCH="$BRANCH"
                done
            else
                local BOUNDARIES
                # shellcheck disable=SC2206
                BOUNDARIES=(${ACTION//:/ })

                info "Merging ${BOUNDARIES[0]} up to ${BOUNDARIES[1]}"
                local BRANCH
                local PREVIOUS_BRANCH=""
                local SKIP=1
                for BRANCH in "${ENV_BRANCHES[@]}"; do
                    if [[ "$BRANCH" == "${BOUNDARIES[0]}" ]]; then
                        SKIP=0
                    fi

                    if [[ "$SKIP" == 0 ]]; then
                        git -C "$PROJECT_DIR/htdocs" switch "$BRANCH"
                        git -C "$PROJECT_DIR/htdocs" pull

                        if [[ -n "$PREVIOUS_BRANCH" ]]; then
                            git -C "$PROJECT_DIR/htdocs" merge "$PREVIOUS_BRANCH"
                            git -C "$PROJECT_DIR/htdocs" push
                        fi

                        if [[ "$BRANCH" == "${BOUNDARIES[1]}" ]]; then
                            break
                        fi
                    fi

                    PREVIOUS_BRANCH="$BRANCH"
                done
            fi
        done
}

function parseArguments() {
    if [[ "$1" == "control"  ]]; then
        # skip plugin command itself
        shift
    fi

    if [[ $# -eq 0 ]]; then
        # show help page as no parameters where given
        _help
        exit 1
    fi

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --dir|-d)
                PROJECT_DIR=$(realpath "$2")
                shift 2
                ;;
            add-deploy-config)
                checkDir
                _addDeployConfig
                exit 0
                ;;
            build)
                checkDir
                shift
                dockerCompose build "$@"
                exit 0
                ;;
            create-control-script)
                checkDir
                shift
                _createControlScript "$@"
                exit 0
                ;;
            cap)
                checkDir
                shift
                _cap "$@"
                exit 0
                ;;
            console)
                checkDir
                _console "${2:-php}"
                exit 0
                ;;
            deploy)
                checkDir
                shift
                _deploy "$@"
                exit 0
                ;;
            merge)
                checkDir
                shift
                _merge "$@"
                exit 0
                ;;
            help)
                _help
                exit 0
                ;;
            init)
                checkDir
                if [[ -z $(find "$PROJECT_DIR" -mindepth 1 -print -quit) ]]; then
                    _init
                    exit 0
                else
                    critical "Current directory is not empty"
                    exit 1
                fi
                ;;
            pull)
                checkDir
                dockerCompose pull
                exit 0
                ;;
            pull-ingress)
                checkDir
                dockerComposeIngress pull
                exit 0
                ;;
            restart)
                checkDir
                dockerCompose down
                dockerCompose up -d
                exit 0
                ;;
            restart-ingress)
                checkDir
                dockerComposeIngress down
                dockerComposeIngress up -d
                exit 0
                ;;
            start)
                checkDir
                dockerCompose up -d
                exit 0
                ;;
            start-ingress)
                checkDir
                dockerComposeIngress up -d
                exit 0
                ;;
            status)
                checkDir
                dockerCompose ps
                exit 0
                ;;
            status-ingress)
                checkDir
                dockerComposeIngress ps
                exit 0
                ;;
            stop)
                checkDir
                dockerCompose down
                exit 0
                ;;
            stop-ingress)
                checkDir
                dockerComposeIngress down
                exit 0
                ;;
            update)
                checkDir
                _update
                exit 0
                ;;
            version)
                checkDir
                headline "IK Docker Control $SERVICE"
                info "Version: $SERVICE"
                exit 0
                ;;
            *)
                checkDir
                COMMAND=$1
                shift
                if [[ -f "${PROJECT_DIR}/control-scripts/${COMMAND}.sh" ]]; then
                    "${PROJECT_DIR}/control-scripts/${COMMAND}.sh" "$@"
                    exit 0
                else
                    critical "Invalid parameter: $COMMAND"
                    newline
                    _help
                    exit 1
                fi
                ;;
        esac
    done
}

function select_docker_service() {
    local SERVICES
    SERVICES="db php"

    local SERVICE_MAP
    # shellcheck disable=SC2034
    declare -A SERVICE_MAP=()
    local SERVICE_ORDER
    # shellcheck disable=SC2034
    declare -a SERVICE_ORDER=()

    IFS="$DEFAULT_IFS"
    for SERVICE in $SERVICES; do
        # shellcheck disable=SC2034
        SERVICE_MAP["$SERVICE"]="$SERVICE"
        SERVICE_ORDER+=("$SERVICE")
    done

    choose "Service" SERVICE_MAP SERVICE_ORDER
}

function select_php_version() {
    local SERVICES
    SERVICES="7.4 7.4-oci 8.2 8.2-oci 8.4 8.4-oci"

    local SERVICE_MAP
    # shellcheck disable=SC2034
    declare -A SERVICE_MAP=()
    local SERVICE_ORDER
    # shellcheck disable=SC2034
    declare -a SERVICE_ORDER=()

    IFS="$DEFAULT_IFS"
    for SERVICE in $SERVICES; do
        # shellcheck disable=SC2034
        SERVICE_MAP["$SERVICE"]="$SERVICE"
        SERVICE_ORDER+=("$SERVICE")
    done

    choose "PHP Version" SERVICE_MAP SERVICE_ORDER
}

