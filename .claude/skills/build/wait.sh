#!/bin/sh
# Waits until pull request $1 merges or needs its author, then prints why it stopped.
# Exit 0: merged. Exit 1: closed, a failed check, a canceled required check, a
# conflict, requested changes, the PR left the merge queue, or the query failed.
#
# Only the latest run of each workflow counts, and in it the latest run of each job,
# so a new run or a rerun replaces the old one. A canceled run counts only on a
# required check: the Review workflow cancels its own `gate` runs, and the `review`
# status, not `gate`, is the required check.
set -u
q='query($n:Int!){repository(owner:"synnaxlabs",name:"foundation"){
  pullRequest(number:$n){state mergeable reviewDecision isInMergeQueue
  autoMergeRequest{enabledAt}
  commits(last:1){nodes{commit{statusCheckRollup{contexts(first:100){nodes{
    __typename
    ... on CheckRun{name databaseId conclusion isRequired(pullRequestNumber:$n)
      checkSuite{workflowRun{databaseId workflow{name}}}}
    ... on StatusContext{state}}}}}}}}}}'
jq='if .errors then
    if any(.errors[]; .type == "RATE_LIMITED") then "waiting"
    else "has a query error: \(.errors[0].message)" end
  else .data.repository.pullRequest |
  (.commits.nodes[0].commit.statusCheckRollup.contexts.nodes // []) as $nodes |
  ([$nodes[] | select(.__typename == "CheckRun")]
    | group_by(.checkSuite.workflowRun.workflow.name)
    | map(max_by(.checkSuite.workflowRun.databaseId).checkSuite.workflowRun.databaseId
      as $run
      | map(select(.checkSuite.workflowRun.databaseId == $run))
      | group_by(.name) | map(max_by(.databaseId)))
    | add // [])
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
  else "left the merge queue" end end'
while :; do
  # A GraphQL error comes on stdout with a nonzero exit; a network error gives no
  # stdout, and the script waits through it.
  a=$(gh api graphql -F n="$1" -f query="$q" 2>/dev/null)
  s=$(printf '%s' "$a" | jq -r "$jq" 2>/dev/null)
  case ${s:-waiting} in
    merged) echo "#$1 merged"; exit 0 ;;
    waiting) sleep 120 ;;
    *) echo "#$1 $s"; exit 1 ;;
  esac
done
