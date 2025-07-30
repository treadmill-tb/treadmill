#!/usr/bin/env bash

METHOD="$1"
URI="$2"
shift 2

AUTHORIZATION_HEADER="Authorization: Bearer B1oy2ko1wVdGKbvKc/9dKi7ggZYLTLzdm2As4CWV15fyuzvHsbBQOvnN+/RpB7OvVJjRYhldlSY4iFsNZq5XpO8fXiqRN6O/gn+nP5cA1J6ox2d2jV32TGzahTZAQZUFwIsI11Mye+Jus97L1e+l3O/0yBt/sywoJFFwkUVOFX8="

http -v "${METHOD}" "http://localhost:8080/${URI}" "$AUTHORIZATION_HEADER" "$@"
