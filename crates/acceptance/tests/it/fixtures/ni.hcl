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

connector "dev" {
  kind = "ni"
  node = "edge"
  address = "dev"
  read "dev.p" {
    physical = "Dev1/ai0"
  }
  command "dev.q" {
    physical = "Dev1/ao0"
  }
}
