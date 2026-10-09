channel "dev.time" { kind = "index" }
channel "dev.p" {
  data_type = "f64"
  index = "dev.time"
}
channel "dev.q_time" { kind = "index" }
channel "dev.q" {
  data_type = "f64"
  index = "dev.q_time"
}
placement "dev" {
  select = ["dev", "dev.*"]
  home   = "edge"
}

connector "dev" {
  kind = "modbus"
  node = "edge"
  transport = "rtu"
  address = "dev"
  read "dev.p" {
    input_register = 0
  }
  command "dev.q" {
    holding_register = 1
  }
}
