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
  kind = "modbus"
  transport = "tcp"
  address = "dev"
  read "dev.p" {
    input_register = 0
  }
  command "dev.q" {
    holding_register = 1
  }
}
