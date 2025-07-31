#!/bin/bash

# shellcheck disable=SC2034
DEFAULT_IFS=$IFS

function _initUtils() {
    local GUM_EXECUTABLE
    GUM_EXECUTABLE=$(which gum)

    if [[ -z "$GUM_EXECUTABLE" ]]; then
        warning "Gum executable not found in PATH, using build in version."
        GUM_EXECUTABLE="$DIR/bin/gum"
    fi
}

function choose() {
    local HEADER
    local OPTION
    local HEIGHT
    HEADER=$(text "$1")
    local -n _OPTIONS_MAP="$2"
    local -n _OPTIONS_ORDER="$3"

    HEIGHT=$((${#_OPTIONS_ORDER[@]} + 2))
    if [ "$HEIGHT" -gt 22 ]; then
        HEIGHT=22
    fi

    OPTION=$("$GUM_EXECUTABLE" choose --header="$HEADER" --height="$HEIGHT" "${_OPTIONS_ORDER[@]}")

    if [ "${_OPTIONS_MAP[$OPTION]}" == 255 ]; then
        return 255
    else
        echo -n "${_OPTIONS_MAP[$OPTION]}"
    fi
}

function choose_multiple() {
    local HEADER
    local OPTIONS
    local HEIGHT
    HEADER=$(text "$1")
    local -n _OPTIONS_MAP="$2"
    local -n _OPTIONS_ORDER="$3"

    HEIGHT=$((${#_OPTIONS_ORDER[@]} + 2))
    if [ "$HEIGHT" -gt 22 ]; then
        HEIGHT=22
    fi

    OPTIONS=$("$GUM_EXECUTABLE" choose --no-limit --header="$HEADER" --height="$HEIGHT" "${_OPTIONS_ORDER[@]}")

    if [ "$OPTIONS" == "" ]; then
      return 255
    fi

    for OPTION in $OPTIONS; do
      echo "${_OPTIONS_MAP[$OPTION]}"
    done
}

function confirm() {
    local DEFAULT="true"
    local QUESTION=""

    while [[ $# -gt 0 ]]; do
        case $1 in
            -n)
                DEFAULT="false"
                shift
                ;;
            *)
                QUESTION="$1"
                shift
                ;;
        esac
    done

    if "$GUM_EXECUTABLE" confirm "${QUESTION}" --default="$DEFAULT"; then
        RESULT="y"
    else
        RESULT="n"
    fi
    echo $RESULT
}

function wait_for_keypress() {
    "$GUM_EXECUTABLE" input --placeholder "Press Enter to continue..." --prompt "" --no-show-help
}

function critical() {
    text -f 9 "$@"
}

function debug() {
    text '{{ Foreground "8" (Blink "'"$1"'") }}'
}

function fatal() {
    critical "$1"
    exit 1
}

function headline() {
    local MESSAGE
    MESSAGE=$(text "$1")

    "$GUM_EXECUTABLE" style --foreground="0" --background="2" --border=double --border-background="2" --padding="1 2" --width=78 --align="center" "$MESSAGE"

    return 0
}

function info() {
    text -f 12 "$@"
}

function input() {
    local ALLOW_EMPTY=1
    local DEFAULT_VALUE=""
    local HEADER
    local ECHO=1
    local PARAM
    local PLACEHOLDER

    while [[ $# -gt 0 ]]; do
        case $1 in
            -r | --reference)
                local -n OUTPUT="$2"
                ECHO=0
                shift 2
                ;;
            -d | --default-value)
                DEFAULT_VALUE="$2"
                shift 2
                ;;
            -l | --label)
                HEADER="$(text "$2")"
                shift 2
                ;;
            -n | --not-empty)
                ALLOW_EMPTY=0
                shift
                ;;
            -p | --placeholder)
                PLACEHOLDER="$2"
                shift 2
                ;;
            *)
                critical "unknown option: $1"
                ;;
        esac
    done

    if [ -z "$PLACEHOLDER" ] && [ -n "$DEFAULT_VALUE" ]; then
        PLACEHOLDER="$DEFAULT_VALUE"
    fi

    OUTPUT=""
    while [ -z "$OUTPUT" ]; do
        OUTPUT=$("$GUM_EXECUTABLE" input --header.foreground="12" --header="$HEADER" --placeholder="$PLACEHOLDER")
        if [ -z "$OUTPUT" ] && [ -n "$DEFAULT_VALUE" ]; then
            OUTPUT="$DEFAULT_VALUE"
        fi
        if [ "$ALLOW_EMPTY" -eq 1 ]; then
            break
        fi
    done

    if [ "$ECHO" -eq 1 ]; then
        echo -n "$OUTPUT"
    fi
}

function menu() {
    local HEADER
    local ACTION
    HEADER=$1
    local -n _ACTIONS_MAP="$2"
    local -n _ACTIONS_ORDER="$3"

    while true; do
        ACTION=$(choose "$HEADER" _ACTIONS_MAP _ACTIONS_ORDER)
        if [ "$?" == 255 ]; then
            break
        else
            $ACTION
        fi
    done
}

function newline() {
    echo
}

function prompt() {
    local MESSAGE=$1
    echo -e "$MESSAGE"
}

function select_file() {
    local FILES_DIR="$1"

    sudo "$GUM_EXECUTABLE" file --height 5 "$FILES_DIR"
}

function sub_headline() {
    local MESSAGE
    MESSAGE=$(text "$1")

    newline
    "$GUM_EXECUTABLE" style --foreground="0" --background="5" --italic --width=80 --align="center" "$MESSAGE"
}

function text() {
    local MESSAGE
    local ARG
    local FG
    local BG

    while [[ $# -gt 0 ]]; do
        case "$1" in
            -f|--foreground)
                FG=$2
                shift 2
                ;;

            -b|--background)
                BG=$2
                shift 2
                ;;

            *)
                if [[ "$1" == \{\{* ]] || { [[ -z "$FG" ]] && [[ -z "$BG" ]]; }; then
                    MESSAGE="${MESSAGE} $1"
                else
                    if [[ -n "$FG" ]] && [[ -n "$BG" ]]; then
                        MESSAGE="${MESSAGE}"' {{ Color "'"${FG}"'" "'"${BG}"'" "'"$1"'" }}'
                    else
                        if [[ -n "$FG" ]]; then
                            MESSAGE="${MESSAGE}"' {{ Foreground "'"${FG}"'" "'"$1"'" }}'
                        fi

                        if [[ -n "$BG" ]]; then
                            MESSAGE="${MESSAGE}"' {{ Background "'"${BG}"'" "'"$1"'" }}'
                        fi
                    fi
                fi
                shift
                ;;
        esac
    done
    MESSAGE=$(echo -e "${MESSAGE}" | sed -e 's/^[[:space:]]*//') # trim spaces

    "$GUM_EXECUTABLE" format -t template "$MESSAGE"
    newline
}

function warning() {
    local MESSAGE
    MESSAGE=$(text "$1")

    newline
    "$GUM_EXECUTABLE" style --foreground="11" "$MESSAGE"
}


