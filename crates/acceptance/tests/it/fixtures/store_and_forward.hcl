channel "edge.value" { data_type = "f64" }

connector "influx" {
  kind = "influx"
  node = "cloud"
  select = "edge.*"
  reader {
    name = "influx"
    mode = "complete"
    hold = "2h"
  }
}
