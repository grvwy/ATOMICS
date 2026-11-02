#!/usr/bin/env perl
# 200 coloured build-log lines per second, like a busy agent tile.
use strict; use warnings; use Time::HiRes qw(time sleep);
$| = 1;
my ($n, $start) = (0, time);
while (1) {
  my $c = 31 + $n % 7;
  printf "\e[%dm%6d\e[0m \e[1mbuild:\e[0m compiling crate_%d src/module_%d.rs \e[2m%s\e[0m\n", $c, $n, $n % 97, $n % 13, '-' x (20 + $n % 40);
  $n++;
  my $due = $start + $n / 200;
  my $wait = $due - time;
  sleep $wait if $wait > 0;
}
