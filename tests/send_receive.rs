use async_metronome::{assert_tick, await_tick};
use futures::{channel::mpsc, SinkExt, StreamExt};

#[test]
fn test_send_receive() {
    let test = async {
        let (mut sender, mut receiver) = mpsc::channel::<usize>(1);

        let sender = async move {
            assert_tick!(0);
            sender.send(42).await.unwrap();
            sender.send(17).await.unwrap();
            assert_tick!(1);
        };
        let receiver = async move {
            assert_tick!(0);
            await_tick!(1);
            receiver.next().await;
            receiver.next().await;
        };
        let sender = async_metronome::spawn(sender);
        let receiver = async_metronome::spawn(receiver);
        sender.await;
        receiver.await;
    };

    async_metronome::run(test);
}
