channel "site.time" { kind = "index" }
channel "site.temp" {
  data_type = "f64"
  index = "site.time"
}
placement "site" {
  select = "site.*"
  home   = "cloud"
}
