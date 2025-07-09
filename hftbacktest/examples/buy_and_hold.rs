use std::fmt::Debug;

use hftbacktest::backtest::models::{L3FIFOQueueModel, TradingQtyFeeModel};
use hftbacktest::backtest::{L3AssetBuilder, MultiAssetSingleExchangeBacktest};
use hftbacktest::{
    backtest::{
        Backtest, ExchangeKind,
        assettype::LinearAsset,
        data::DataSource,
        models::{CommonFees, ConstantLatency},
    },
    depth::ROIVectorMarketDepth,
    prelude::{Bot, *},
};

pub fn algo<I, MD>(hbt: &mut I)
where
    MD: MarketDepth,
    I: Bot<MD>,
    <I as Bot<MD>>::Error: Debug,
{
    let mut order_id = 0u64;

    // Running interval in nanoseconds
    while let Ok(ElapseResult::Ok) = hbt.elapse(100_000_000) {
        hbt.clear_inactive_orders(Some(0));

        let depth = hbt.depth(0);
        let position = hbt.position(0);

        let new_order_id = if position == 0.0 {
            match depth.best_bid_tick().try_into() {
                Ok(id) => id,
                Err(_) => 0u64,
            }
        } else {
            0u64
        };
        let order_price = depth.best_bid();

        if new_order_id != order_id {
            let orders = hbt.orders(0);
            let cancel_order_ids: Vec<u64> = orders
                .values()
                .filter(|order| order.cancellable())
                .map(|order| order.order_id)
                .collect();
            for order_id in cancel_order_ids {
                hbt.cancel(0, order_id, false).unwrap();
            }
        }

        let orders = hbt.orders(0);
        if new_order_id > 0 && orders.is_empty() {
            order_id = new_order_id;
            hbt.submit_buy_order(
                0,
                order_id,
                order_price,
                1.0,
                TimeInForce::GTC,
                OrdType::Limit,
                false,
            )
            .unwrap();
        }
    }
}

fn main() {
    let data: Vec<DataSource<Event>> = vec![DataSource::File(format!(
        "C:/code/hftbacktest/hftbacktest/npz_data/BTCM5_a_20250513_l3.npz"
    ))];
    // let data: Vec<DataSource<Event>> = vec![DataSource::File(format!("C:/code/hftbacktest/hftbacktest/npz_data/002594_20250609.npz"))];
    // let data = (20250513..20250514)
    //     .map(|date| DataSource::File(format!("C:/code/hftbacktest/hftbacktest/npz_data/BTCM5_a_{date}_l3.npz")))
    //     .collect();
    // let data = (20250609..20250610)
    //     .map(|date| DataSource::File(format!("C:/code/hftbacktest/hftbacktest/npz_data/002594_{date}.npz")))
    //     .collect();

    println!("{:?}", data);

    let mut hbt = Backtest::builder()
        .add_asset(
            L3AssetBuilder::new()
                .data(data)
                .latency_model(ConstantLatency::new(10_000_000, 10_000_000))
                .asset_type(LinearAsset::new(5.0))
                .fee_model(TradingQtyFeeModel::new(CommonFees::new(5.0, 5.0)))
                .last_trades_capacity(0)
                .exchange(ExchangeKind::NoPartialFillExchange)
                .queue_model(L3FIFOQueueModel::new())
                .depth(|| ROIVectorMarketDepth::new(5.0, 1.0, 0.0, 150000.0))
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();

    algo(&mut hbt);
    hbt.close().unwrap();
}
