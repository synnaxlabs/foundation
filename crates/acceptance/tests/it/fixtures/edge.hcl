channel "edge.time" {
  kind = "index"
  error = "edge.time_error"
}
channel "edge.time_error" {
  data_type = "u64"
  index = "edge.time"
}
channel "edge.value" {
  data_type = "f64"
  index = "edge.time"
}
