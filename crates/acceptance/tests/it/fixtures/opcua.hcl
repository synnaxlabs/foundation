channel "dev.p" { data_type = "f64" }
channel "dev.p_cmd" { data_type = "f64" }

connector "dev" {
  kind = "opcua"
  address = "dev"
  read "p" {
    node = "ns=2;s=p"
  }
  command "p_cmd" {
    node = "ns=2;s=p"
  }
}
