channel "dev.time" { kind = "index" }
channel "dev.p" {
  data_type = "f64"
  index = "dev.time"
}
channel "dev.q" {
  data_type = "f64"
  index = "dev.time"
}

connector "dev" {
  kind = "opcua"
  address = "dev"
  read "dev.p" {
    node = "ns=2;s=p"
  }
  command "dev.q" {
    node = "ns=2;s=q"
  }
}
