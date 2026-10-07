#!/bin/sh
# Waits until pull request $1 merges or needs its author, then prints why it stopped.
# Exit 0: merged. Exit 1: closed, a failed check, a canceled required check, a
# conflict, requested changes, or the PR left the merge queue.
#
# A check run counts by its latest run per workflow and job, so a rerun replaces it.
# A canceled run counts only on a required check: the Review workflow cancels its own
# `gate` runs, and the `review` status, not `gate`, is the required check.
set -u
q='query($n:Int!){repository(owner:"synnaxlabs",name:"foundation"){
  pullRequest(number:$n){state mergeable reviewDecision isInMergeQueue
  autoMergeRequest{enabledAt}
  commits(last:1){nodes{commit{statusCheckRollup{contexts(first:100){nodes{
    __typename
    ... on CheckRun{name databaseId conclusion isRequired(pullRequestNumber:$n)
      checkSuite{workflowRun{workflow{name}}}}
    ... on StatusContext{state}}}}}}}}}}'
jq='.data.repository.pullRequest |
  (.commits.nodes[0].commit.statusCheckRollup.contexts.nodes // []) as $nodes |
  ([$nodes[] | select(.__typename == "CheckRun")]
    | group_by([.checkSuite.workflowRun.workflow.name, .name])
    | map(max_by(.databaseId)))
  + [$nodes[] | select(.__typename == "StatusContext") | {conclusion: .state}]
  as $checks |
  if .state == "MERGED" then "merged"
  elif .state != "OPEN" then "closed"
  elif any($checks[]; .conclusion
    | IN("FAILURE", "ERROR", "TIMED_OUT", "STARTUP_FAILURE", "ACTION_REQUIRED"))
    then "has a failed check"
  elif any($checks[]; .conclusion == "CANCELLED" and .isRequired)
    then "has a canceled required check"
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
