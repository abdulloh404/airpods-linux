# airpods-power

`airpods_power.ko` exposes `airpods_left`, `airpods_right`, and `airpods_case`
through Linux's `power_supply` class. UPower can therefore enumerate both
earbuds and the charging case separately.

The module accepts one protocol-version-2 update on `/dev/airpods_power`:

```text
version = 2,
left_present, left_capacity, left_charging,
right_present, right_capacity, right_charging,
case_present, case_capacity, case_charging, case_stale
```

Boolean fields must be `0` or `1`, and capacity must be between `0` and `100`
when the matching battery is present. A stale case remains visible with its
last reported percentage, but it is never reported as charging. This lets
UPower keep a percentage-based battery icon instead of replacing it with its
missing-battery icon. If no update arrives for 90 seconds, the module marks all
three batteries absent so UPower cannot retain values after a daemon crash.

Building the module does not install or load it. Loading and persistence will
be handled with packaging later.
