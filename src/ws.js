import ws from 'k6/ws';
import { check } from 'k6';

export let options = {
  vus: 10000,
  duration: '30s', // Increased from 2s to 30s to observe garbage collection
};

let total = 0;

export default function () {
  let url = 'ws://127.0.0.1:8080';
  let messageReceived = false;

  let res = ws.connect(url, function (socket) {
    socket.on('open', function () {
      socket.send(JSON.stringify({ num: 42 }));
    });

    socket.on('message', function (message) {
      console.log(`Received: ${message}`);
      total += 1;
      console.log(`Current total: ${total}`);
      messageReceived = true;

      // Close the connection manually after receiving the message
      socket.close();
    });

    // Set a timeout to detect if no message is received
    socket.setTimeout(function() {
      if (!messageReceived) {
        console.log('No message received within timeout period');
      }
      // Connection will be closed if timeout occurs without message
      if (!messageReceived) {
        socket.close();
      }
    }, 5000);
  });

  check(res, {
    'Connected successfully': (r) => r && r.status === 101,
    'Message received': () => messageReceived
  });
}
