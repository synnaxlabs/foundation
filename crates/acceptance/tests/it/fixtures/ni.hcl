channel "dev.p" { data_type = "f64" }
channel "dev.p_cmd" { data_type = "f64" }

connector "dev" {
  kind = "ni"
  address = "dev"
  read "p" {
    physical = "Dev1/ai0"
  }
  command "p_cmd" {
    physical = "Dev1/ai0"
  }
}
