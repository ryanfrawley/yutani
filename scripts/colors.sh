#!/bin/sh
# Print the 16-color and 256-color palettes for visual inspection.
# Usage: ./scripts/colors.sh [16|256|all]   (default: all)

set -eu

esc=$(printf '\033')
reset="${esc}[0m"

print_16() {
    printf '== 16 colors (SGR 30-37 / 90-97, 40-47 / 100-107) ==\n'

    printf 'fg dim:    '
    for n in 0 1 2 3 4 5 6 7; do
        printf '%s[3%dm %2d %s' "$esc" "$n" "$((30 + n))" "$reset"
    done
    printf '\n'

    printf 'fg bright: '
    for n in 0 1 2 3 4 5 6 7; do
        printf '%s[9%dm %2d %s' "$esc" "$n" "$((90 + n))" "$reset"
    done
    printf '\n'

    printf 'bg dim:    '
    for n in 0 1 2 3 4 5 6 7; do
        printf '%s[4%dm %2d %s' "$esc" "$n" "$((40 + n))" "$reset"
    done
    printf '\n'

    printf 'bg bright: '
    for n in 0 1 2 3 4 5 6 7; do
        printf '%s[10%dm %2d %s' "$esc" "$n" "$((100 + n))" "$reset"
    done
    printf '\n'
}

print_256() {
    printf '\n== 256 colors (SGR 38;5;n / 48;5;n) ==\n'

    printf '\nbase 16 (0-15):\n'
    n=0
    while [ "$n" -lt 16 ]; do
        printf '%s[48;5;%dm %3d %s' "$esc" "$n" "$n" "$reset"
        n=$((n + 1))
        [ $((n % 8)) -eq 0 ] && printf '\n'
    done

    printf '\n6x6x6 cube (16-231):\n'
    n=16
    while [ "$n" -lt 232 ]; do
        printf '%s[48;5;%dm %3d %s' "$esc" "$n" "$n" "$reset"
        n=$((n + 1))
        [ $(((n - 16) % 36)) -eq 0 ] && printf '\n'
    done

    printf '\ngrayscale ramp (232-255):\n'
    n=232
    while [ "$n" -lt 256 ]; do
        printf '%s[48;5;%dm %3d %s' "$esc" "$n" "$n" "$reset"
        n=$((n + 1))
    done
    printf '\n'

    printf '\nfg over default bg:\n'
    n=0
    while [ "$n" -lt 256 ]; do
        printf '%s[38;5;%dm%3d %s' "$esc" "$n" "$n" "$reset"
        n=$((n + 1))
        [ $((n % 16)) -eq 0 ] && printf '\n'
    done
}

mode=${1:-all}
case "$mode" in
    16)  print_16 ;;
    256) print_256 ;;
    all) print_16; print_256 ;;
    *)   printf 'usage: %s [16|256|all]\n' "$0" >&2; exit 2 ;;
esac
