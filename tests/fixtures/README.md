# Pacman version comparison fixtures

`pacman-vercmptest.sh` is the unmodified pacman vercmp test suite from the
[devkitPro pacman mirror](https://raw.githubusercontent.com/devkitPro/pacman/cf473bcfbd275044250fa6ce3703dd7059a52273/test/util/vercmptest.sh), pinned to commit `cf473bcfbd275044250fa6ce3703dd7059a52273`.
Its copyright and GPL-2.0-or-later notice are retained in the file. Maple tests
read all 46 `tap_runtest` rows and also check their reversed comparisons (92
assertions). The script itself is not executed, so tests do not require Bash or
pacman's TAP helpers.

A separate test compares Maple with an installed `vercmp`, when available.
Set `MAPLE_REQUIRE_VERCMP_TESTS=1` to require that differential test.
