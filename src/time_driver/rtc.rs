//! RTC Driver.
use core::cell::RefCell;
use core::sync::atomic::{compiler_fence, AtomicU32, Ordering};

use critical_section::CriticalSection;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_time_driver::Driver;
use embassy_time_queue_utils::Queue;

use super::AlarmState;
use crate::interrupt::InterruptExt;
use crate::{interrupt, pac};

// SAFETY: This function allows access to the RTC peripheral's register block without ownership checks.
//         If a register is to be accessed from multiple locations (e.g. an interrupt), access to it
//         must be synchronized using a critical section or other synchronization mechanism.
unsafe fn rtc() -> &'static pac::rtc::RegisterBlock {
    unsafe { &*pac::Rtc::ptr() }
}

/// Calculate the timestamp from the period count and the tick count.
///
/// To get `now()`, `period` is read first, then `counter` is read. If the counter value matches
/// the expected range for the `period` parity, we're done. If it doesn't, this means that
/// a new period start has raced us between reading `period` and `counter`, so we assume the `counter` value
/// corresponds to the next period.
///
/// the 1kHz RTC counter is 16 bits and RTC doesn't have separate compare channels,
/// so using a 32 bit GPREG0-2 as counter, compare, and int_en
/// `period` is a 32bit integer, gpreg 'counter' is 31 bits plus the parity bit for overflow detection
#[cfg(feature = "time-driver-rtc")]
fn calc_now(period: u32, counter: u32) -> u64 {
    ((period as u64) << 31) + ((counter ^ ((period & 1) << 31)) as u64)
}

#[cfg(feature = "time-driver-rtc")]
embassy_time_driver::time_driver_impl!(static DRIVER: Rtc = Rtc {
    period: AtomicU32::new(0),
    alarms:  Mutex::const_new(CriticalSectionRawMutex::new(), AlarmState::new()),
    queue: Mutex::new(RefCell::new(Queue::new())),
});

#[cfg(feature = "time-driver-rtc")]
struct Rtc {
    /// Number of 2^31 periods elapsed since boot.
    period: AtomicU32,
    /// Timestamp at which to fire alarm. u64::MAX if no alarm is scheduled.
    alarms: Mutex<CriticalSectionRawMutex, AlarmState>,
    queue: Mutex<CriticalSectionRawMutex, RefCell<Queue>>,
}

#[cfg(feature = "time-driver-rtc")]
impl Rtc {
    /// Access the GPREG0 register to use it as a 31-bit counter.
    #[inline]
    unsafe fn counter_reg(&self) -> &pac::rtc::Gpreg {
        rtc().gpreg(0)
    }

    /// Access the GPREG1 register to use it as a compare register for triggering alarms.
    #[inline]
    unsafe fn compare_reg(&self) -> &pac::rtc::Gpreg {
        rtc().gpreg(1)
    }

    /// Access the GPREG2 register to use it to enable or disable interrupts (int_en).
    #[inline]
    unsafe fn int_en_reg(&self) -> &pac::rtc::Gpreg {
        rtc().gpreg(2)
    }

    fn init(&'static self, irq_prio: crate::interrupt::Priority) {
        let r = unsafe { rtc() };
        // enable RTC int (1kHz since subsecond doesn't generate an int)
        r.ctrl().modify(|_r, w| w.rtc1khz_en().set_bit());
        // TODO: low power support. line above is leaving out write to .wakedpd_en().set_bit())
        // which enables wake from deep power down

        // safety: Writing to the gregs is always considered unsafe, gpreg1 is used
        // as a compare register for triggering an alarm so to avoid unnecessary triggers
        // after initialization, this is set to 0x:FFFF_FFFF
        unsafe { self.compare_reg() }.write(|w| unsafe { w.gpdata().bits(u32::MAX) });

        // Zero the counter register to prevent overflow.  Overflow is not generally expected to happen,
        // but in cases where users are switching between different compile-time implementations of the time
        // driver, the counter register may have a left-over value from the previous firmware that may have
        // been using this register for a different purpose, which may trigger overflow.
        // Since this is just used to track the number of ticks since boot, it is safe to zero it out.
        unsafe { self.counter_reg() }.write(|w| unsafe { w.gpdata().bits(0) });
        // safety: writing a value to the 1kHz RTC wake counter is always considered unsafe.
        // The following loads 10 into the count-down timer.
        r.wake().write(|w| unsafe { w.bits(0xA) });
        interrupt::RTC.set_priority(irq_prio);
        unsafe { interrupt::RTC.enable() };
    }

    #[cfg(feature = "rt")]
    fn on_interrupt(&self) {
        let r = unsafe { rtc() };
        // This interrupt fires every 10 ticks of the 1kHz RTC high res clk and adds
        // 10 to the 31 bit counter gpreg0. The 32nd bit is used for parity detection
        // This is done to avoid needing to calculate # of ticks spent on interrupt
        // handlers to recalibrate the clock between interrupts
        //
        // TODO: this is admittedly not great for power that we're generating this
        // many interrupts, will probably get updated in future iterations.
        if r.ctrl().read().wake1khz().bit_is_set() {
            r.ctrl().modify(|_r, w| w.wake1khz().set_bit());
            // safety: writing a value to the 1kHz RTC wake counter is always considered unsafe.
            // The following reloads 10 into the count-down timer after it triggers an int.
            // The countdown begins anew after the write so time can continue to be measured.
            r.wake().write(|w| unsafe { w.bits(0xA) });
            if (unsafe { self.counter_reg().read().bits() } + 0xA) > 0x8000_0000 {
                // if we're going to "overflow", increase the period
                self.next_period();
                let rollover_diff = 0x8000_0000 - (unsafe { self.counter_reg().read().bits() } + 0xA);
                // safety: writing to gpregs is always considered unsafe. In order to
                // not "lose" time when incrementing the period, gpreg0, the extended
                // counter, is restarted at the # of ticks it would overflow by
                unsafe { self.counter_reg() }.write(|w| unsafe { w.bits(rollover_diff) });
            } else {
                unsafe { self.counter_reg() }.modify(|r, w| unsafe { w.bits(r.bits() + 0xA) });
            }
        }

        critical_section::with(|cs| {
            // gpreg2 as an "int_en" set by next_period(). This is
            // 1 when the timestamp for the alarm deadline expires
            // before the counter register overflows again.
            if unsafe { self.int_en_reg().read().gpdata().bits() == 1 } {
                // gpreg0 is our extended counter register, check if
                // our counter is larger than the compare value
                if unsafe { self.counter_reg().read().bits() } > unsafe { self.compare_reg().read().bits() } {
                    self.trigger_alarm(cs);
                }
            }
        })
    }

    #[cfg(feature = "rt")]
    fn next_period(&self) {
        critical_section::with(|cs| {
            let period = self
                .period
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |p| Some(p + 1))
                .unwrap_or_else(|p| {
                    trace!("Unable to increment period. Time is now inaccurate");
                    // TODO: additional error handling beyond logging

                    p
                });
            let t = (period as u64) << 31;

            let alarm = &self.alarms.borrow(cs);
            let at = alarm.timestamp.get();
            if at < t + 0xc000_0000 {
                // safety: writing to gpregs is always unsafe, gpreg2 is an alarm
                // enable. If the alarm must trigger within the next period, then
                // just enable it. `set_alarm` has already set the correct CC val.
                unsafe { self.int_en_reg() }.write(|w| unsafe { w.gpdata().bits(1) });
            }
        })
    }

    #[must_use]
    fn set_alarm(&self, cs: CriticalSection, timestamp: u64) -> bool {
        let alarm = self.alarms.borrow(cs);
        alarm.timestamp.set(timestamp);

        let t = self.now();
        if timestamp <= t {
            // safety: Writing to the gpregs is always unsafe, gpreg2 is
            // always just used as the alarm enable for the timer driver.
            // If alarm timestamp has passed the alarm will not fire.
            // Disarm the alarm and return `false` to indicate that.
            unsafe { self.int_en_reg() }.write(|w| unsafe { w.gpdata().bits(0) });

            alarm.timestamp.set(u64::MAX);

            return false;
        }

        // If it hasn't triggered yet, setup it by writing to the compare field
        // An alarm can be delayed, but this is allowed by the Alarm trait contract.
        // What's not allowed is triggering alarms *before* their scheduled time,
        let safe_timestamp = timestamp.max(t + 10); //t+3 was done for nrf chip, choosing 10

        // safety: writing to the gregs is always unsafe. When a new alarm is set,
        // the compare register, gpreg1, is set to the last 31 bits of the timestamp
        // as the 32nd and final bit is used for the parity check in `next_period`
        // `period` will be used for the upper bits in a timestamp comparison.
        unsafe { self.compare_reg() }.modify(|_r, w| unsafe { w.bits(safe_timestamp as u32 & 0x7FFF_FFFF) });

        // The following checks that the difference in timestamp is less than the overflow period
        let diff = timestamp - t;
        if diff < 0xc000_0000 {
            // this is 0b11 << (30). NRF chip used 23 bit periods and checked against 0b11<<22

            // safety: writing to the gpregs is always unsafe. If the alarm
            // must trigger within the next period, set the "int enable"
            unsafe { self.int_en_reg() }.write(|w| unsafe { w.gpdata().bits(1) });
        } else {
            // safety: writing to the gpregs is always unsafe. If alarm must trigger
            // some time after the current period, too far in the future, don't setup
            // the alarm enable, gpreg2, yet. It will be setup later by `next_period`.
            unsafe { self.int_en_reg() }.write(|w| unsafe { w.gpdata().bits(0) });
        }

        true
    }

    #[cfg(feature = "rt")]
    fn trigger_alarm(&self, cs: CriticalSection) {
        let mut next = self.queue.borrow(cs).borrow_mut().next_expiration(self.now());
        while !self.set_alarm(cs, next) {
            next = self.queue.borrow(cs).borrow_mut().next_expiration(self.now());
        }
    }
}

#[cfg(feature = "time-driver-rtc")]
impl Driver for Rtc {
    fn now(&self) -> u64 {
        // `period` MUST be read before `counter`, see comment at the top for details.
        let period = self.period.load(Ordering::Acquire);
        compiler_fence(Ordering::Acquire);
        let counter = unsafe { self.counter_reg() }.read().bits();
        calc_now(period, counter)
    }

    fn schedule_wake(&self, at: u64, waker: &core::task::Waker) {
        critical_section::with(|cs| {
            let mut queue = self.queue.borrow(cs).borrow_mut();

            if queue.schedule_wake(at, waker) {
                let mut next = queue.next_expiration(self.now());
                while !self.set_alarm(cs, next) {
                    next = queue.next_expiration(self.now());
                }
            }
        })
    }
}

#[cfg(all(feature = "rt", feature = "time-driver-rtc"))]
#[allow(non_snake_case)]
#[interrupt]
fn RTC() {
    DRIVER.on_interrupt()
}

pub(crate) fn init(irq_prio: crate::interrupt::Priority) {
    DRIVER.init(irq_prio)
}
