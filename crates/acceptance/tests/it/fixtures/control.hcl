policy "operator" {
  subject = "operator"
  allow = ["read", "write"]
  names = ["dev.*"]
}

policy "viewer" {
  subject = "viewer"
  allow = ["read"]
  names = ["dev.*"]
}
