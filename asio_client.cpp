#include <cstdlib>
#include <cstring>
#include <iostream>
#include <asio.hpp>
#include <mutex>
#include <vector>


using asio::ip::tcp;
using asio::read;
using asio::buffer;

#ifdef _MSC_VER
    #define PACK( __Declaration__ ) __pragma( pack(push, 1) ) __Declaration__ __pragma( pack(pop) )
#else
    #define PACK( __Declaration__ ) __Declaration__ __attribute__((__packed__))
#endif

PACK(
    struct H12 {
        uint16_t device_id;
        uint32_t unixtime;
        uint16_t ms;
        int32_t pkt_id;
    }
);

int main(int argc, char* argv[])
{
    uint32_t pkt_len = 5452;
    uint32_t header_len = 12;
    try {
        if (argc != 3) {
            std::cerr << "Usage: asio_client <host> <port>\n";
            return 1;
        }

        asio::io_context io_context;
        asio::signal_set signals(io_context, SIGINT, SIGTERM);
        signals.async_wait([&](auto, auto){ io_context.stop(); });

        tcp::socket socket(io_context);
        tcp::resolver resolver(io_context);
        asio::connect(socket, resolver.resolve(argv[1], argv[2]));

        H12 header;
        std::vector<int16_t> pkt_buf((pkt_len - header_len) / 2);

        for(;;) {
            read(socket, buffer(&header, header_len));
            std::printf("%d\t%d\t%d\n", header.pkt_id, header.unixtime, header.ms);

            read(socket, buffer(pkt_buf.data(), pkt_buf.size() * 2));
        }
    }
    catch (std::exception& e) {
        std::cerr << "Exception: " << e.what() << "\n";
    }

    return 0;
}
