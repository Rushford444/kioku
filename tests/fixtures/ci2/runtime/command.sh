#!/bin/sh
printf "native stdout"
printf "native stderr" >&2
exit "$1"
