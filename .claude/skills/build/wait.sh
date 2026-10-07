#!/bin/sh
# Waits until pull request $1 merges or needs its author, then prints why it stopped.
# Exit 0: merged. Exit 1: closed, a failed check, a conflict, requested changes, or the
# PR left the merge queue.
set -u
q='query($n:Int!){repository(owner:"synnaxlabs",name:"foundation"){
  pullRequest(number:$n){state mergeable reviewDecision isInMergeQueue
  autoMergeRequest{enabledAt}
  commits(last:1){nodes{commit{statusCheckRollup{state}}}}}}}'
jq='.data.repository.pullRequest |
  if .state == "MERGED" then "merged"
  elif .state != "OPEN" then "closed"
  elif (.commits.nodes[0].commit.statusCheckRollup.state // "")
    | IN("FAILURE", "ERROR") then "has a failed check"
  elif .mergeable == "CONFLICTING" then "conflicts with main"
  elif .reviewDecision == "CHANGES_REQUESTED" then "has requested changes"
  elif .isInMergeQueue or .autoMergeRequest != null then "waiting"
  else "left the merge queue" end'
while :; do
  s=$(gh api graphql -F n="$1" -f query="$q" --jq "$jq") || s=waiting
  case $s in
    merged) echo "#$1 merged"; exit 0 ;;
    waiting) sleep 120 ;;
    *) echo "#$1 $s"; exit 1 ;;
  esac
done
