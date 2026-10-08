- **K5 + REGION LOCKED + K5 REVISION** There is one mesh. A region keeps changing its
  own definitions while cut off. A region changes its own voters. The parent only
  creates or removes a region, or forces a takeover (admin on the parent, `--force`,
  epoch bump; nodes reject commits from an old epoch). The parent cannot veto. Access
  across regions is ordinary access policy. A change that spans regions commits per
  region in dependency order. Supersedes: D7 linked meshes, K5 parent-owned voters.
