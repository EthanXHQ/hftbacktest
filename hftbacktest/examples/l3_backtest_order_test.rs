use std::fmt::Debug;

use hftbacktest::{
    backtest::{
        Backtest, DataSource, ExchangeKind, L3AssetBuilder,
        assettype::LinearAsset,
        models::{CommonFees, ConstantLatency, L3FIFOQueueModel, TradingQtyFeeModel},
    },
    depth::{MarketDepth, ROIVectorMarketDepth},
    types::{Bot, ElapseResult, Event, OrdType, TimeInForce},
};

pub fn algo<I, MD>(hbt: &mut I)
where
    MD: MarketDepth,
    I: Bot<MD>,
    <I as Bot<MD>>::Error: Debug,
{
    let mut order_id = 0u64;
    let elapse_time = 1_000_000_000;
    let mut has_bought = false;

    while let Ok(ElapseResult::Ok) = hbt.elapse(elapse_time) {
        hbt.clear_inactive_orders(Some(0));

        let depth = hbt.depth(0);
        let position = hbt.position(0);
        let state = hbt.state_values(0);
        let current_time = hbt.current_timestamp();

        println!(
            "时间: {}, 仓位: {}, 状态: {:?}",
            current_time, position, state
        );

        if current_time < 1750901400000000000 {
            // println!("orders {:?}", hbt.orders(0));
            println!("未开盘，跳过");
            continue;
        }

        let order_price = depth.best_ask();
        println!("最佳卖价: {}", order_price);

        let new_order_id = if position == 0.0 {
            match depth.best_bid_tick().try_into() {
                Ok(id) => id,
                Err(_) => 0u64,
            }
        } else {
            0u64
        };

        let order_price = depth.best_bid();
        println!("最佳买价: {}", order_price);

        // 简化买入逻辑：如果没有仓位且没有活跃订单，则买入
        if position == 0.0 && !has_bought {
            let orders = hbt.orders(0);
            println!("orders {:?}", orders);
            if orders.is_empty() {
                println!(
                    "提交买入订单: ID={}, 价格={}, 数量=1.0",
                    order_id, order_price
                );

                match hbt.submit_buy_order(
                    0,
                    order_id,
                    order_price,
                    1.0,
                    TimeInForce::GTC,
                    OrdType::Limit,
                    false,
                ) {
                    Ok(_) => {
                        println!("买入订单提交成功");
                        order_id += 1; // 递增订单ID
                    }
                    Err(e) => {
                        println!("买入订单提交失败: {:?}", e);
                    }
                }
            } else {
                println!("存在活跃订单，等待执行");
            }
        } else if position > 0.0 {
            has_bought = true;
            println!("已持有仓位: {}", position);
        }

        // SELL ORDER
        if hbt.current_timestamp() == 1750901402000000000 {
            match hbt.submit_sell_order(
                0,
                order_id,
                order_price,
                1.0,
                TimeInForce::GTC,
                OrdType::Limit,
                false,
            ) {
                Ok(_) => {
                    println!("卖出订单提交成功");
                    order_id += 1;
                }
                Err(e) => {
                    println!("卖出订单提交失败: {:?}", e);
                }
            }
        }

        // CANCEL ORDER
        // if hbt.current_timestamp() == 1750901402000000000 {
        //     match hbt.cancel(0, 0u64, false) {
        //         Ok(_) => {
        //             println!("取消订单提交成功");
        //         }
        //         Err(e) => {
        //             println!("取消订单提交失败: {:?}", e);
        //         }
        //     }
        // }

        // MODIFY ORDER
        // if hbt.current_timestamp() == 1750901404000000000 {
        //     match hbt.modify(0, 1u64, 343.3, 2.0, false) {
        //         Ok(_) => {
        //             println!("修改订单提交成功");
        //         }
        //         Err(e) => {
        //             println!("修改订单提交失败: {:?}", e);
        //         }
        //     }
        // }

        // 检查是否有成交
        let orders = hbt.orders(0);
        if orders.is_empty() && position > 0.0 {
            println!("订单已成交，当前仓位: {}", position);
        }

        if hbt.current_timestamp() > 1750901415000000000 {
            break;
        }
    }
}

fn main() {
    println!("=== now test backtest order ===");

    let data: Vec<DataSource<Event>> = vec![DataSource::File(format!(
        "C:/code/my_hftbacktest/hftbacktest/hftbacktest/npz_data/002594_20250626.npz"
    ))];

    let mut hbt = Backtest::builder()
        .add_asset(
            L3AssetBuilder::new()
                .data(data)
                .latency_model(ConstantLatency::new(0, 0))
                .asset_type(LinearAsset::new(1.0))
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

    // TODO ADD MODIFY CANCEL orders
    algo(&mut hbt);
    hbt.close().unwrap();
}
