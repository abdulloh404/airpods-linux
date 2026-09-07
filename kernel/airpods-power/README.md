# airpods-power

`airpods_power.ko` exposes `airpods_left` and `airpods_right` through Linux's
`power_supply` class. UPower can therefore enumerate each earbud separately.

The module accepts one seven-byte update on `/dev/airpods_power`:

```text
version,
left_present, left_capacity, left_charging,
right_present, right_capacity, right_charging
```

The protocol version is currently `1`. Boolean fields must be `0` or `1`, and
capacity must be between `0` and `100` when the matching earbud is present. If
no update arrives for 90 seconds, the module marks both earbuds absent so
UPower cannot retain stale percentages after a daemon crash.

Building the module does not install or load it. Loading and persistence will
be handled with packaging later.
