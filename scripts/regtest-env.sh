# Source this before running the regtest tests:  . scripts/regtest-env.sh
# Override ZAKURAD_BIN / ZECD_BIN to point at your own builds.
export ZECD_REGTEST_NODE=zakura
export ZAKURAD_BIN=${ZAKURAD_BIN:-/mnt/roman-dev-2-data/zakura-v188-target/release/zakurad}
export ZECD_BIN=${ZECD_BIN:-/mnt/roman-dev-2-data/zecd-pr1-target/release/zecd}
export ZECD_REGTEST_NU63_HEIGHT=8
export ZECD_REGTEST_NU7_HEIGHT=12
