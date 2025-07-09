use std::cmp;
use std::fmt::Debug;

use hftbacktest::{
    backtest::{
        Backtest, DataSource, ExchangeKind, L3AssetBuilder,
        assettype::LinearAsset,
        models::{CommonFees, ConstantLatency, L3FIFOQueueModel, TradingQtyFeeModel},
    },
    depth::{MarketDepth, ROIVectorMarketDepth},
    types::{Bot, ElapseResult, Event},
};

pub fn print_bbo<I, MD>(hbt: &mut I)
where
    MD: MarketDepth,
    I: Bot<MD>,
    <I as Bot<MD>>::Error: Debug,
{
    while let Ok(ElapseResult::Ok) = hbt.elapse(1_000_000_000) {
        let depth = hbt.depth(0);

        println!(
            "{} , best bid: {:.2} , best ask: {:.2}",
            hbt.current_timestamp(),
            depth.best_bid(),
            depth.best_ask()
        );

        // 输出到开盘
        // 9:25:00
        if hbt.current_timestamp() > 1750901105000000000 {
            break;
        }

        // 9:30:00
        // if hbt.current_timestamp() > 1750901405000000000 {
        //     break;
        // }
    }
}

pub fn print_5depth<I, MD>(hbt: &mut I)
where
    I: Bot<MD>,
    MD: MarketDepth,
    <I as Bot<MD>>::Error: Debug,
{
    let mut elapse_time = 1_000_000_000;
    while let Ok(ElapseResult::Ok) = hbt.elapse(elapse_time) {
        let depth = hbt.depth(0);

        println!("\n========== 订单簿 ==========");
        println!("时间: {}", hbt.current_timestamp());
        println!("----------------------------");

        // 收集卖盘数据（卖三、卖二、卖一）
        let mut asks = Vec::new();
        let mut i = 0;
        for tick_price in depth.best_ask_tick()..=(depth.best_ask_tick() as f64 * 1.1) as i64 {
            let qty = depth.ask_qty_at_tick(tick_price);
            if qty > 0.0 {
                asks.push((tick_price, qty));
                i += 1;
                if i == 5 {
                    break;
                }
            }
        }

        // 倒序打印卖盘（从卖三到卖一）
        asks.reverse();
        for (idx, (tick_price, qty)) in asks.iter().enumerate() {
            println!(
                "卖{} {:>10.2} @ {:>10.2}",
                5 - idx,
                qty,
                *tick_price as f64 * depth.tick_size()
            );
        }

        println!("----------------------------");

        // 打印买盘（买一、买二、买三）
        let mut i = 0;
        for tick_price in ((cmp::max((depth.best_bid_tick() as f64 * 0.9) as i64, 0))
            ..=depth.best_bid_tick())
            .rev()
        {
            let qty = depth.bid_qty_at_tick(tick_price);
            if qty > 0.0 {
                println!(
                    "买{} {:>10.2} @ {:>10.2}",
                    i + 1,
                    qty,
                    tick_price as f64 * depth.tick_size()
                );
                i += 1;
                if i == 5 {
                    break;
                }
            }
        }
        println!("============================\n");

        // 输出到开盘
        // 9:15:00
        // if hbt.current_timestamp() > 1750900505000000000 {
        //     break;
        // }

        // 9:25:00
        // if hbt.current_timestamp() > 1750901102000000000 {
        //     break;
        // }

        // 9:30:00
        // if hbt.current_timestamp() >= 1750901400000000000 {
        //     elapse_time = 10_000_000;
        //     println!("{:?}",depth.ask_qty_at_tick(34411));
        // }
        // // if hbt.current_timestamp()== 1750901399000000000{
        // //     println!("{:?}",depth.ask_qty_at_tick(34510));
        // // }
        // // if hbt.current_timestamp()== 1750901400000000000{
        // //     println!("{:?}",depth.ask_qty_at_tick(34510));
        // // }
        // if hbt.current_timestamp() > 1750901400030000000 {
        //     println!("{:?}",depth.ask_qty_at_tick(34411));
        //     break;
        // }
        // println!("{:?}", depth.ask_qty_at_tick(34411 as i64));

        // if hbt.current_timestamp() > 1750908598000000000 {
        //     elapse_time = 10_000_000;
        // }

        // 11:30:00
        // if hbt.current_timestamp() > 1750908602000000000 {
        //     break;
        // }

        // 14:56:57
        // if hbt.current_timestamp() > 1750921017000000000 {
        //     break;
        // }

        // 15:00:00
        // if hbt.current_timestamp() > 1750921201000000000 {
        //     break;
        // }
    }
}

fn main() {
    println!("Printing best bid & ask:");

    let data: Vec<DataSource<Event>> = vec![DataSource::File(format!(
        "C:/code/my_hftbacktest/hftbacktest/hftbacktest/npz_data/002594_20250626.npz"
    ))];

    let mut hbt = Backtest::builder()
        .add_asset(
            L3AssetBuilder::new()
                .data(data)
                .latency_model(ConstantLatency::new(0, 0))
                .asset_type(LinearAsset::new(5.0))
                .fee_model(TradingQtyFeeModel::new(CommonFees::new(0.0, 0.0)))
                .last_trades_capacity(0)
                .exchange(ExchangeKind::PartialFillExchange)
                .queue_model(L3FIFOQueueModel::new())
                .depth(|| ROIVectorMarketDepth::new(0.01, 100.0, 0.0, 150000.0))
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();

    // print_bbo(&mut hbt);
    print_5depth(&mut hbt);
    hbt.close().unwrap();
}
