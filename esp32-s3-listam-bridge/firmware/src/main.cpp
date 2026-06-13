#include <Arduino.h>

// Helper to print memory stats to verify N16R8 config
void printMemoryStats() {
    Serial.println("--- SYSTEM STATS ---");
    Serial.printf("Total Heap: %d bytes\n", ESP.getHeapSize());
    Serial.printf("Free Heap: %d bytes\n", ESP.getFreeHeap());
    
    #ifdef BOARD_HAS_PSRAM
    Serial.printf("PSRAM detected: %s\n", psramFound() ? "YES" : "NO");
    if (psramFound()) {
        Serial.printf("Total PSRAM: %d bytes\n", ESP.getPsramSize());
        Serial.printf("Free PSRAM: %d bytes\n", ESP.getFreePsram());
    }
    #else
    Serial.println("PSRAM: Not enabled in compilation flags");
    #endif
    
    Serial.printf("Flash size: %d bytes\n", ESP.getFlashChipSize());
    Serial.println("--------------------");
}

void setup() {
    // Turn off built-in RGB LED if it exists (usually GPIO 48 on S3)
    #ifdef RGB_BUILTIN
    neopixelWrite(RGB_BUILTIN, 0, 0, 0);
    #endif

    // Start serial communication
    Serial.begin(115200);
    
    // Wait for native USB serial connection (max 3 seconds)
    while (!Serial && millis() < 3000) {
        delay(10);
    }
    
    Serial.println("\nREADY:S3");
    printMemoryStats();
}

void loop() {
    static uint32_t lastMessageTime = 0;
    static int itemIndex = 0;
    
    // List of items to cycle through for mock list adds
    const char* items[] = {
        "Milk from ESP32-S3",
        "Bread from ESP32-S3",
        "Apples from ESP32-S3",
        "Butter from ESP32-S3",
        "Coffee from ESP32-S3"
    };
    const int numItems = sizeof(items) / sizeof(items[0]);
    
    // Send a new item add command every 15 seconds
    if (millis() - lastMessageTime > 15000) {
        lastMessageTime = millis();
        
        Serial.printf("ADD:%s\n", items[itemIndex]);
        
        // Cycle to next item
        itemIndex = (itemIndex + 1) % numItems;
    }
    
    // Yield to let ESP32 background tasks run smoothly
    delay(10);
}
