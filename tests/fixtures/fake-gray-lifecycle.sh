#!/bin/sh
# Fake `gray gateway lifecycle` for lifecycle::tests::core_answers_win.
case "$3" in
  boot) echo '{"notice":"from core"}' ;;
  stop) echo '{"restart":true,"active_notice":"a","home_notice":"h"}' ;;
esac
