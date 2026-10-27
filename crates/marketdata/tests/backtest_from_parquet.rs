use chrono::NaiveDate;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;
use st_core::Event;
use st_data::{SyntheticConfig, VecEventSource, generate_daily_bars};
use st_engine::{Backtest, FillModel, SimulatedBroker};
use st_marketdata::{BarFilter, BarWriter, DailyBar, read_bars, to_core_bar};
use st_risk::{RiskLimits, RiskManager};
use st_strategies::SmaCrossover;

#[test]
fn parquet_bars_drive_a_backtest() {
    let synthetic = generate_daily_bars(&SyntheticConfig {
        symbols: 10,
        days: 200,
        start: NaiveDate::from_ymd_opt(2024, 1, 2).unwrap(),
        seed: 11,
    });
    let daily: Vec<DailyBar> = synthetic
        .iter()
        .map(|b| DailyBar {
            symbol: b.symbol.to_string(),
            date: b.end.with_timezone(&chrono_tz::America::New_York).date_naive(),
            open: b.open.to_f64().unwrap(),
            high: b.high.to_f64().unwrap(),
            low: b.low.to_f64().unwrap(),
            close: b.close.to_f64().unwrap(),
            volume: b.volume.to_f64().unwrap(),
            trade_count: None,
            vwap: None,
        })
        .collect();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bars.parquet");
    let mut writer = BarWriter::create(&path).unwrap();
    for chunk in daily.chunks(333) {
        writer.write(chunk).unwrap();
    }
    writer.finish().unwrap();

    let bars: Vec<_> = read_bars(&path, &BarFilter::default()).unwrap().iter().filter_map(to_core_bar).collect();
    assert_eq!(bars, synthetic, "Parquet round trip must reproduce the engine's bars exactly");

    let report = Backtest::new(
        VecEventSource::new(bars.into_iter().map(Event::bar).collect()),
        SmaCrossover::new(10, 30, dec!(0.05)).unwrap(),
        SimulatedBroker::new(FillModel::default()),
        RiskManager::new(RiskLimits { min_avg_dollar_volume: dec!(0), ..RiskLimits::default() }),
        dec!(100_000),
    )
    .run()
    .unwrap();
    assert!(report.metrics.num_fills > 0);
    assert_eq!(report.equity_curve.len(), 200);
}
