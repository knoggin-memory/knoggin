#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-only
#
# Checks that code, comments and docs stand alone: a reader needs nothing outside
# this repository to follow them. Fails on text that matches the patterns below in
# every tracked text file except lockfiles (phrases also across line breaks), in
# each commit message of an optional range, and in PR_TITLE / PR_BODY when set.
# --pr-text checks only PR_TITLE / PR_BODY. Known false hits go in
# scripts/check-wording.allow.
#
# Usage: scripts/check-wording.sh [--pr-text] [<base>..<head>]   (or set WORDING_RANGE)
set -eu

cd "$(git rev-parse --show-toplevel)"

pr_only=false
if [ "${1:-}" = --pr-text ]; then
    pr_only=true
    shift
fi
range="${1:-${WORDING_RANGE:-}}"

# Short IDs match upper case only: lower case q4 (model names), p95 (percentiles)
# and m0 (variables) are ordinary words here. Phrases match in any case, and a
# space in one stands for any run of whitespace, line breaks and comment leaders.
IDS='\b(?:S?Q|M|I)[0-9]{1,2}\b|\bP[0-9]{2}\b' # wording-pattern
PHRASES='\bper the designs?\b|\bthe specs?\b|\btab (?:\x22|\xE2\x80\x9C)|spec-deviations|\bmilestone[\s#-]*[0-9]|\bmeasurements? M[0-9]' # wording-pattern
ALLOW=scripts/check-wording.allow
export IDS PHRASES ALLOW

# Prints "<label>:<line>:<text>" for each hit in the named files, or in stdin
# labelled $LABEL. An allow-list line "<path>:<text>" removes <text> from hits in
# <path>; a hit stays when the rest of its line still matches. Lines of this
# script that end in the marker comment are skipped.
# shellcheck disable=SC2016 # Perl, not shell.
scanner='
use strict;
use warnings;
my $ids = qr/$ENV{IDS}/;
my $sep = q{\s+(?:(?://[/!]?|#|\*|--)\s*)?};
(my $p = $ENV{PHRASES}) =~ s/ /$sep/g;
my $phrases = qr/$p/i;
my %allow;
open(my $fh, "<", $ENV{ALLOW}) or die "$ENV{ALLOW}: $!\n";
while (<$fh>) {
    chomp;
    next if /^#/ || !/:/;
    my ($path, $text) = split /:/, $_, 2;
    push @{ $allow{$path} }, $text;
}
close $fh;
@ARGV = ("-") unless @ARGV;
local $/;
while (<>) {
    my $label = $ARGV eq "-" ? $ENV{LABEL} : $ARGV;
    s/^.*# wording-pattern$//mg if $label eq "scripts/check-wording.sh";
    my (@hits, %seen);
    my $n = 0;
    for my $line (split /\n/) {
        $n++;
        push @hits, [$n, $line] if $line =~ $ids;
    }
    while (/$phrases/g) {
        my ($start, $end) = ($-[0], $+[0]);
        my $from = rindex($_, "\n", $start - 1) + 1;
        my $to = index($_, "\n", $end);
        $to = length($_) if $to < 0;
        (my $text = substr($_, $from, $to - $from)) =~ s/\s*\n\s*/ /g;
        push @hits, [1 + (substr($_, 0, $start) =~ tr/\n//), $text];
    }
    for my $hit (@hits) {
        my ($line, $text) = @$hit;
        my $rest = $text;
        $rest =~ s/\Q$_\E/ /g for @{ $allow{$label} || [] };
        next unless $rest =~ $ids || $rest =~ $phrases;
        print "$label:$line:$text\n" unless $seen{"$line:$text"}++;
    }
}
'

hits=$(mktemp)
files=$(mktemp)
trap 'rm -f "$hits" "$files"' EXIT

scan() {
    LABEL="$1" perl -e "$scanner" >>"$hits"
}

if ! $pr_only; then
    # The allow-list holds fragments of the lines it excuses.
    git grep --no-color -I -l -z -e '' -- . ":(exclude)$ALLOW" \
        ':(exclude,glob)**/*.lock' ':(exclude,glob)**/*.lock.hcl' ':(exclude,glob)**/pnpm-lock.yaml' \
        >"$files"
    xargs -0 perl -e "$scanner" <"$files" >>"$hits"

    if [ -n "$range" ]; then
        # A pull request always holds at least one commit, so an empty range means a wrong range.
        commits=$(git rev-list "$range")
        if [ -z "$commits" ]; then
            echo "$range holds no commits." >&2
            exit 1
        fi
        for commit in $commits; do
            git log -1 --format=%B "$commit" | scan "commit $(git rev-parse --short "$commit")"
        done
    fi
fi
if [ -n "${PR_TITLE+set}" ]; then
    printf '%s\n' "$PR_TITLE" | scan "PR title"
fi
if [ -n "${PR_BODY+set}" ]; then
    printf '%s\n' "$PR_BODY" | scan "PR body"
fi

if [ -s "$hits" ]; then
    cat "$hits" >&2
    echo "Hits: $(wc -l <"$hits" | tr -d ' '). Reword each line above so it stands alone;" >&2
    echo "add a genuine false hit to $ALLOW." >&2
    exit 1
fi
if $pr_only; then
    echo "No outside citations found in the pull request text."
else
    echo "No outside citations found${range:+ (tree and commits in $range)}."
fi
