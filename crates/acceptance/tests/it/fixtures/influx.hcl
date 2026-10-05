connector "influx" {
  kind = "influx"
  node = "cloud"
  address = "influx"
  select = "edge.*"
  reader {
    name = "influx"
    mode = "complete"
    hold = "2h"
  }
}
