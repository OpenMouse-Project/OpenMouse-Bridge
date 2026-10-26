use super::*;

#[derive(Default)]
struct FakeIo {
    calls: Vec<String>,
    fail_reads: bool,
    fail_writes: usize,
    invalid_baseline: bool,
}

impl AutomationIo for FakeIo {
    fn read(&mut self, id: &str) -> Result<Baseline> {
        self.calls.push(format!("read:{id}"));
        if self.fail_reads {
            anyhow::bail!("unsupported readback");
        }
        Ok(Baseline {
            identity: DeviceIdentity {
                vendor_id: 1,
                product_id: 2,
                path: id.into(),
                serial_number: "serial".into(),
                interface: Some(0),
            },
            dpi: if self.invalid_baseline { 0 } else { 800 },
            polling_rate_hz: 1000,
        })
    }
    fn write(&mut self, id: &str, baseline: &Baseline, settings: &ProfileSettings) -> Result<()> {
        assert_eq!(baseline.identity.path, id);
        self.calls.push(format!(
            "write:{id}:{:?}:{:?}",
            settings.dpi, settings.polling_rate_hz
        ));
        if self.fail_writes > 0 {
            self.fail_writes -= 1;
            anyhow::bail!("write failed after possible partial hardware change");
        }
        Ok(())
    }
}

fn profile(id: &str, dpi: Option<u32>, hz: Option<u32>) -> ApplicationProfile {
    serde_json::from_value(serde_json::json!({"application":{"name":"Game","path":"/game","executable":"game"},"device":{"id":id,"name":"Mouse"},"settings":{"dpi":dpi,"pollingRateHz":hz},"enabled":true})).unwrap()
}

#[test]
fn unchanged_request_does_not_rewrite_and_empty_profile_does_not_read() {
    let mut automation = NativeAutomation::default();
    let mut io = FakeIo::default();
    let game = profile("a", Some(1600), None);
    automation
        .reconcile(Some(&profile("a", None, None)), &mut io)
        .unwrap();
    assert!(io.calls.is_empty());
    automation.reconcile(Some(&game), &mut io).unwrap();
    automation.reconcile(Some(&game), &mut io).unwrap();
    assert_eq!(io.calls, ["read:a", "write:a:Some(1600):None"]);
}

#[test]
fn invalid_or_unreadable_baseline_never_writes() {
    for invalid in [false, true] {
        let mut automation = NativeAutomation::default();
        let mut io = FakeIo {
            fail_reads: !invalid,
            invalid_baseline: invalid,
            ..FakeIo::default()
        };
        assert!(
            automation
                .reconcile(Some(&profile("a", Some(1600), None)), &mut io)
                .is_err()
        );
        assert_eq!(io.calls, ["read:a"]);
        assert!(automation.entries.is_empty());
    }
}

#[test]
fn failed_partial_apply_is_restored_even_if_same_request_remains() {
    let mut automation = NativeAutomation::default();
    let mut io = FakeIo {
        fail_writes: 1,
        ..FakeIo::default()
    };
    let game = profile("a", Some(1600), Some(2000));
    assert!(automation.reconcile(Some(&game), &mut io).is_err());
    let original_error = automation.apply_error.clone();
    assert_eq!(automation.attempted.as_ref(), Some(&game));
    let _ = automation.reconcile(Some(&game), &mut io);
    let _ = automation.reconcile(Some(&game), &mut io);
    assert_eq!(automation.apply_error, original_error);
    assert_eq!(
        io.calls,
        [
            "read:a",
            "write:a:Some(1600):Some(2000)",
            "write:a:Some(800):Some(1000)"
        ]
    );
    assert!(automation.entries.is_empty());
}

#[test]
fn owned_baseline_is_pending_when_final_target_ends_before_hid_access() {
    let mut automation = NativeAutomation::default();
    let mut io = FakeIo::default();
    let game = profile("a", Some(1600), None);
    automation.reconcile(Some(&game), &mut io).unwrap();
    assert_eq!(automation.pending_restores_for(Some(&game)), 0);
    assert_eq!(automation.pending_restores_for(None), 1);
    assert_eq!(io.calls.len(), 2);
}

#[test]
fn failed_restoration_retains_baseline_and_waits_before_retry() {
    let mut automation = NativeAutomation::default();
    let mut io = FakeIo::default();
    let now = Instant::now();
    automation
        .reconcile_at(Some(&profile("a", Some(1600), None)), &mut io, now)
        .unwrap();
    io.fail_writes = 20;
    assert!(automation.reconcile_at(None, &mut io, now).is_err());
    for millis in [0, 1, 500, 999] {
        assert!(
            automation
                .reconcile_at(None, &mut io, now + Duration::from_millis(millis))
                .is_err()
        );
    }
    assert_eq!(io.calls.len(), 3);
    assert_eq!(automation.entries["a"].baseline.dpi, 800);
    assert_eq!(automation.pending_restores_for(None), 1);
    io.fail_writes = 0;
    automation
        .reconcile_at(None, &mut io, now + Duration::from_secs(1))
        .unwrap();
    assert_eq!(io.calls.len(), 4);
    assert_eq!(io.calls.last().unwrap(), "write:a:Some(800):None");
    assert!(automation.entries.is_empty());
}

#[test]
fn restoration_failure_delays_increase_to_cap_without_abandoning_baseline() {
    let mut automation = NativeAutomation::default();
    let mut io = FakeIo::default();
    let mut now = Instant::now();
    automation
        .reconcile_at(Some(&profile("a", Some(1600), None)), &mut io, now)
        .unwrap();
    io.fail_writes = 10;
    for (index, seconds) in [1, 2, 4, 8, 16, 32, 60, 60, 60, 60].into_iter().enumerate() {
        assert!(automation.reconcile_at(None, &mut io, now).is_err());
        let calls = io.calls.len();
        assert_eq!(calls, index + 3);
        assert_eq!(automation.entries["a"].baseline.dpi, 800);
        assert_eq!(
            automation.entries["a"].baseline.identity.serial_number,
            "serial"
        );
        assert_eq!(automation.pending_restores_for(None), 1);
        now += Duration::from_secs(seconds);
        assert!(
            automation
                .reconcile_at(None, &mut io, now - Duration::from_nanos(1))
                .is_err()
        );
        assert_eq!(io.calls.len(), calls, "retry before {seconds}s deadline");
    }
    automation.reconcile_at(None, &mut io, now).unwrap();
    assert_eq!(io.calls.len(), 13);
    assert_eq!(io.calls.last().unwrap(), "write:a:Some(800):None");
    assert!(automation.entries.is_empty());
}

#[test]
fn restoration_deadlines_are_independent_per_device() {
    let mut automation = NativeAutomation::default();
    let mut io = FakeIo::default();
    let now = Instant::now();
    automation
        .reconcile_at(Some(&profile("a", Some(1600), None)), &mut io, now)
        .unwrap();
    io.fail_writes = 1;
    assert!(
        automation
            .reconcile_at(Some(&profile("b", None, Some(2000))), &mut io, now)
            .is_err()
    );
    assert_eq!(io.calls.len(), 5);
    assert!(
        automation
            .reconcile_at(None, &mut io, now + Duration::from_millis(100))
            .is_err()
    );
    assert_eq!(io.calls.len(), 6);
    assert_eq!(io.calls.last().unwrap(), "write:b:None:Some(1000)");
    assert_eq!(automation.entries.len(), 1);
    assert_eq!(automation.entries["a"].baseline.dpi, 800);
    automation
        .reconcile_at(None, &mut io, now + Duration::from_secs(1))
        .unwrap();
    assert_eq!(io.calls.last().unwrap(), "write:a:Some(800):None");
    assert!(automation.entries.is_empty());
}

#[test]
fn delayed_restoration_preserves_manual_override_and_untouched_ownership() {
    for (dpi, polling, expected) in [
        (true, false, Some("write:Zaunkoenig:M3K:None:Some(1000)")),
        (false, true, Some("write:Zaunkoenig:M3K:Some(800):None")),
        (true, true, None),
    ] {
        let mut automation = NativeAutomation::default();
        let mut io = FakeIo::default();
        let now = Instant::now();
        automation
            .reconcile_at(
                Some(&profile("Zaunkoenig:M3K", Some(1600), Some(2000))),
                &mut io,
                now,
            )
            .unwrap();
        io.fail_writes = 1;
        assert!(automation.reconcile_at(None, &mut io, now).is_err());
        let mut intent = ManualIntent::new();
        intent.invalidate("Zaunkoenig", dpi, polling).unwrap();
        automation.observe_manual_intent(&intent);
        let _ = automation.reconcile_at(None, &mut io, now + Duration::from_millis(999));
        assert_eq!(io.calls.len(), 3);
        let _ = automation.reconcile_at(None, &mut io, now + Duration::from_secs(1));
        if let Some(expected) = expected {
            assert_eq!(io.calls.len(), 4);
            assert_eq!(io.calls.last().unwrap(), expected);
        } else {
            assert_eq!(io.calls.len(), 3);
        }
        assert!(automation.entries.is_empty());
    }
}

#[test]
fn delayed_restoration_retains_all_64_baselines_and_refuses_new_capture() {
    let mut automation = NativeAutomation::default();
    let mut io = FakeIo::default();
    let now = Instant::now();
    let desired = profile("Zaunkoenig:new", Some(1600), None);
    for index in 0..64 {
        automation
            .apply(
                &profile(&format!("Zaunkoenig:{index}"), Some(1600), None),
                &mut io,
            )
            .unwrap();
    }
    io.fail_writes = 64;
    assert!(automation.reconcile_at(None, &mut io, now).is_err());
    let calls = io.calls.len();
    assert_eq!(automation.pending_restores_for(None), 64);
    assert!(
        automation
            .reconcile_at(Some(&desired), &mut io, now + Duration::from_millis(999),)
            .is_err()
    );
    assert_eq!(io.calls.len(), calls);
    assert!(automation.attempted.is_none());
    assert_eq!(automation.entries.len(), 64);
    assert!(
        automation
            .entries
            .values()
            .all(|entry| entry.baseline.dpi == 800)
    );
    automation
        .reconcile_at(Some(&desired), &mut io, now + Duration::from_secs(1))
        .unwrap();
    assert_eq!(io.calls.len(), calls + 66);
    assert_eq!(
        io.calls.last().unwrap(),
        "write:Zaunkoenig:new:Some(1600):None"
    );
    assert_eq!(automation.attempted.as_ref(), Some(&desired));
    assert!(automation.apply_error.is_none());
    automation
        .reconcile_at(None, &mut io, now + Duration::from_secs(2))
        .unwrap();
    assert!(automation.entries.is_empty());
}

#[test]
fn delayed_same_device_restoration_retries_unchanged_desired_profile() {
    let mut automation = NativeAutomation::default();
    let mut io = FakeIo::default();
    let now = Instant::now();
    let desired = profile("a", Some(3200), None);
    automation
        .reconcile_at(Some(&profile("a", Some(1600), None)), &mut io, now)
        .unwrap();
    io.fail_writes = 1;
    assert!(automation.reconcile_at(None, &mut io, now).is_err());
    assert!(
        automation
            .reconcile_at(Some(&desired), &mut io, now + Duration::from_millis(999))
            .is_err()
    );
    assert!(automation.attempted.is_none());
    assert_eq!(io.calls.len(), 3);
    automation
        .reconcile_at(Some(&desired), &mut io, now + Duration::from_secs(1))
        .unwrap();
    assert_eq!(
        &io.calls[3..],
        [
            "write:a:Some(800):None",
            "read:a",
            "write:a:Some(3200):None"
        ]
    );
    assert_eq!(automation.attempted.as_ref(), Some(&desired));
    assert!(automation.apply_error.is_none());
    automation
        .reconcile_at(Some(&desired), &mut io, now + Duration::from_secs(2))
        .unwrap();
    assert_eq!(io.calls.len(), 6);
}

#[test]
fn returning_to_previous_profile_while_deferred_retries_after_restoration() {
    let mut automation = NativeAutomation::default();
    let mut io = FakeIo {
        fail_writes: 1,
        ..Default::default()
    };
    let now = Instant::now();
    let original = profile("a", Some(1600), None);
    assert!(
        automation
            .reconcile_at(Some(&original), &mut io, now)
            .is_err()
    );
    io.fail_writes = 1;
    assert!(
        automation
            .reconcile_at(Some(&profile("a", Some(3200), None)), &mut io, now)
            .is_err()
    );
    assert_eq!(automation.attempted.as_ref(), Some(&original));
    assert!(
        automation
            .reconcile_at(Some(&original), &mut io, now + Duration::from_millis(999))
            .is_err()
    );
    automation
        .reconcile_at(Some(&original), &mut io, now + Duration::from_secs(1))
        .unwrap();
    assert_eq!(io.calls.last().unwrap(), "write:a:Some(1600):None");
    assert!(automation.apply_error.is_none());
}

#[test]
fn restored_previous_device_profile_is_reapplied_after_blocked_switch() {
    let mut automation = NativeAutomation::default();
    let mut io = FakeIo::default();
    let now = Instant::now();
    let a = profile("a", Some(1600), None);
    let b = profile("b", Some(3200), None);
    automation.reconcile_at(Some(&b), &mut io, now).unwrap();
    io.fail_writes = 1;
    assert!(automation.reconcile_at(Some(&a), &mut io, now).is_err());
    let waiting = now + Duration::from_millis(999);
    assert!(automation.reconcile_at(Some(&b), &mut io, waiting).is_err());
    assert!(automation.reconcile_at(Some(&a), &mut io, waiting).is_err());
    assert_eq!(io.calls.last().unwrap(), "write:a:Some(1600):None");
    assert!(automation.apply_error.is_none());
    let calls = io.calls.len();
    automation
        .reconcile_at(Some(&a), &mut io, now + Duration::from_secs(1))
        .unwrap();
    assert_eq!(io.calls.len(), calls + 1);
    assert_eq!(io.calls.last().unwrap(), "write:b:Some(800):None");
    assert!(automation.entries.contains_key("a"));
    assert_eq!(automation.attempted.as_ref(), Some(&a));
    assert!(automation.apply_error.is_none());
}

#[test]
fn explicit_default_preserves_values_and_resets_only_omitted_owned_fields() {
    let mut automation = NativeAutomation::default();
    let mut io = FakeIo::default();
    automation
        .reconcile(Some(&profile("a", Some(1600), Some(2000))), &mut io)
        .unwrap();
    let mut default = profile("a", Some(400), None);
    default.application.name = "Manual default".into();
    automation.reconcile(Some(&default), &mut io).unwrap();
    assert_eq!(io.calls.last().unwrap(), "write:a:Some(400):Some(1000)");
    automation.reconcile(None, &mut io).unwrap();
    assert_eq!(io.calls.last().unwrap(), "write:a:Some(800):Some(1000)");
}

#[test]
fn device_switch_restores_old_identity_before_capturing_new_device() {
    let mut automation = NativeAutomation::default();
    let mut io = FakeIo::default();
    automation
        .reconcile(Some(&profile("a", Some(1600), None)), &mut io)
        .unwrap();
    automation
        .reconcile(Some(&profile("b", Some(3200), None)), &mut io)
        .unwrap();
    automation.reconcile(None, &mut io).unwrap();
    assert_eq!(
        io.calls,
        [
            "read:a",
            "write:a:Some(1600):None",
            "write:a:Some(800):None",
            "read:b",
            "write:b:Some(3200):None",
            "write:b:Some(800):None"
        ]
    );
}

#[test]
fn helper_adapter_binds_read_and_write_to_full_profile_and_captured_identity() {
    let mut requests = Vec::new();
    let captured = serde_json::json!({"identity":{"vendorId":1155,"productId":1,"path":"exact","serialNumber":"serial","interface":2},"dpi":800,"pollingRateHz":1000});
    let mut io = HelperIo(|request: &serde_json::Value| {
        requests.push(request.clone());
        Ok(Some(serde_json::to_vec(&captured).unwrap()))
    });
    let baseline = io.read("Zaunkoenig:M3K").unwrap();
    assert_eq!(baseline.dpi, 800);
    assert_eq!(requests[0]["deviceId"], "Zaunkoenig:M3K");
    assert_eq!(requests[0]["action"], "automatic-read");
    let mut io = HelperIo(|request: &serde_json::Value| {
        assert_eq!(request["identity"]["path"], "exact");
        assert_eq!(request["identity"]["serialNumber"], "serial");
        assert_eq!(request["deviceId"], "Zaunkoenig:M3K");
        assert_eq!(request["action"], "automatic-apply");
        Ok(Some(
            serde_json::to_vec(&serde_json::json!({"identity":captured["identity"]})).unwrap(),
        ))
    });
    io.write(
        "Zaunkoenig:M3K",
        &baseline,
        &profile("a", Some(1600), None).settings,
    )
    .unwrap();
    let mut io = HelperIo(|_: &serde_json::Value| -> Result<Option<Vec<u8>>> {
        panic!("unsupported driver must not launch a helper");
    });
    assert!(io.read("Pulsar:Mouse").is_err());
}

#[test]
fn helper_adapter_rejects_changed_acknowledgement_identity() {
    let mut source = FakeIo::default();
    let baseline = source.read("a").unwrap();
    let mut io = HelperIo(|_: &serde_json::Value| {
        Ok(Some(br#"{"identity":{"vendorId":1,"productId":2,"path":"wrong","serialNumber":"serial","interface":0}}"#.to_vec()))
    });
    assert!(
        io.write(
            "Zaunkoenig:M3K",
            &baseline,
            &profile("a", Some(1600), None).settings
        )
        .is_err()
    );
}

#[test]
fn captures_before_write_and_restores_original_not_handoff() {
    let mut automation = NativeAutomation::default();
    let mut io = FakeIo::default();
    automation
        .reconcile(Some(&profile("a", Some(1600), Some(2000))), &mut io)
        .unwrap();
    automation
        .reconcile(Some(&profile("a", Some(3200), Some(4000))), &mut io)
        .unwrap();
    automation.reconcile(None, &mut io).unwrap();
    assert_eq!(
        io.calls,
        [
            "read:a",
            "write:a:Some(1600):Some(2000)",
            "write:a:Some(3200):Some(4000)",
            "write:a:Some(800):Some(1000)"
        ]
    );
    assert!(automation.entries.is_empty());
}
