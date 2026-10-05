channel "dev.p" { data_type = "f64" }
channel "dev.p_cmd" { data_type = "f64" }

connector "dev" {
  kind = "modbus"
  transport = "tcp"
  address = "dev"
  read "p" {
    register = 0
  }
  command "p_cmd" {
    register = 0
  }
}
