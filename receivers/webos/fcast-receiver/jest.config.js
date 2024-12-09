module.exports = {
    preset: 'ts-jest',
    testEnvironment: 'node',
    testMatch: ['<rootDir>/test/**/*.test.ts'],
    modulePathIgnorePatterns: ["<rootDir>/packaging/fcast/fcast-receiver-linux-x64/resources/app/package.json"],
};