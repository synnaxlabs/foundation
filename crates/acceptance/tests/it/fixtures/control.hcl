access "operator" {
  subjects = "operator"
  select = "dev.*"
  allow = [read, write]
}

access "viewer" {
  subjects = "viewer"
  select = "dev.*"
  allow = [read]
}
