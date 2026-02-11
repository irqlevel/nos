#include <lib/stdlib.h>

namespace Kernel
{

class Parameters
{
public:
    static Parameters& GetInstance()
    {
        static Parameters Instance;
        return Instance;
    }

    bool Parse(const char *cmdline);

    bool IsTraceVga();
    bool IsPanicVga();
    bool IsSmpOff();

    bool IsConsoleSerial();
    bool IsConsoleVga();
    bool IsConsoleBoth();

    Parameters();
    ~Parameters();
private:
    bool ParseParameter(const char *cmdline, size_t start, size_t end);

    enum ConsoleMode {
        ConsoleBoth = 0,
        ConsoleSerialOnly,
        ConsoleVgaOnly,
    };

    char Cmdline[256];
    bool TraceVga;
    bool PanicVga;
    bool SmpOff;
    ConsoleMode ConMode;
};
}